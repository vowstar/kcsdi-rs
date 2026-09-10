// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Main application struct and eframe::App implementation.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::acquisition::{AcquisitionGroup, SweepDelivery};
use crate::config::AppConfig;
use crate::desktop::{self, Page};
use crate::device_worker;
use crate::i18n::{self, Language, StatusMessage, Text};
use crate::panels;
use crate::state::{AppState, ConnectionState, EventEnvelope, S11Display, SweepState, WorkerEvent};
use crate::theme;
use crate::widgets;
use crate::workspace::{TraceDisplay, TraceEditor, TraceSettings};

/// Debounce before persisting config changes to disk.
const CONFIG_SAVE_DELAY: Duration = Duration::from_secs(1);

/// The main kcsdi GUI application.
pub struct KcsdiApp {
    /// Application state shared across all panels.
    pub state: AppState,
    /// Receiver for events from the device worker thread.
    evt_rx: mpsc::Receiver<EventEnvelope>,
    /// Last persisted config snapshot, for change detection.
    last_saved: AppConfig,
    /// When the first unsaved change happened (debounce start).
    dirty_since: Option<Instant>,
    connection_open: bool,
    worker: Option<std::thread::JoinHandle<()>>,
    closing: bool,
}

impl KcsdiApp {
    /// Create a new application instance.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::setup(&cc.egui_ctx);

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::sync_channel(device_worker::EVENT_CAPACITY);

        let cfg = crate::config::load();
        let mut state = AppState {
            cmd_tx: Some(cmd_tx),
            ..AppState::default()
        };
        cfg.apply_to(&mut state);
        let ctx = cc.egui_ctx.clone();
        let shutdown = state.worker_shutdown.clone();
        let preview = state.preview_mailbox.clone();
        let worker = std::thread::Builder::new()
            .name("device-worker".to_string())
            .spawn(move || device_worker::device_worker(cmd_rx, evt_tx, ctx, shutdown, preview))
            .expect("failed to spawn device worker thread");

        Self {
            last_saved: AppConfig::from_state(&state),
            state,
            evt_rx,
            dirty_since: None,
            connection_open: false,
            worker: Some(worker),
            closing: false,
        }
    }

    /// Results belong to a request, but loss of its socket affects the session.
    fn apply_worker_event(&mut self, envelope: EventEnvelope) {
        if envelope.session_id != self.state.session_id {
            return;
        }
        if matches!(
            envelope.event,
            WorkerEvent::SweepTrace(_)
                | WorkerEvent::SweepStopped
                | WorkerEvent::RunProgress(_)
                | WorkerEvent::SourceReport(_)
                | WorkerEvent::CalibrationReport(_)
                | WorkerEvent::Error(_)
        ) && envelope.request_id != self.state.request_id
        {
            return;
        }
        if let WorkerEvent::SweepTrace(delivery) = envelope.event {
            if let Some(cycle) = envelope.cycle_id {
                self.apply_delivery(delivery, cycle);
            }
        } else {
            self.apply_event(envelope.event);
        }
    }

    fn accepts_group(&self, group: &AcquisitionGroup) -> bool {
        self.state.connection == ConnectionState::Connected
            && self.state.function == crate::source_panel::InstrumentFunction::Measurements
            && self.state.any_running()
            && self
                .state
                .active_plan
                .as_ref()
                .is_some_and(|plan| plan.groups.contains(group))
    }

    fn apply_delivery(&mut self, delivery: SweepDelivery, cycle: u64) {
        let group = AcquisitionGroup {
            settings: delivery.snapshot.settings.clone(),
            members: delivery.members,
        };
        if !self.accepts_group(&group)
            || delivery.snapshot.session_id != self.state.session_id
            || !group.settings.accepts(&delivery.snapshot.data)
        {
            return;
        }
        let range = self.state.workspace.range;
        let list = self.state.workspace.requested_list().map(<[u64]>::to_vec);
        for trace in &mut self.state.workspace.traces {
            if !trace.settings.visible
                || !group.members.contains(&trace.id)
                || trace
                    .settings
                    .acquisition_for(&range, list.as_deref())
                    .ok()
                    .as_ref()
                    != Some(&group.settings)
                || !trace.settings.display.accepts(&delivery.snapshot.data)
                || trace.last_completed_cycle.is_some_and(|last| cycle <= last)
            {
                continue;
            }
            trace.last_completed_cycle = Some(cycle);
            if trace
                .preview
                .as_ref()
                .is_some_and(|preview| preview.cycle_id <= cycle)
            {
                trace.preview = None;
            }
            trace.analysis.observe(&delivery.snapshot);
            trace.completed = Some(delivery.snapshot.clone());
            trace.needs_fit = true;
        }
        self.state.status_message = None;
    }

    fn apply_preview(&mut self, preview: crate::preview::PreviewEnvelope) {
        if preview.session_id != self.state.session_id
            || preview.request_id != self.state.request_id
            || !self.accepts_group(&preview.group)
            || preview.data.mode != preview.group.settings.mode()
            || preview.data.format != preview.group.settings.format()
            || preview.data.points.len() > preview.group.settings.points() as usize
        {
            return;
        }
        let range = self.state.workspace.range;
        let list = self.state.workspace.requested_list().map(<[u64]>::to_vec);
        let preview = Arc::new(preview);
        for trace in &mut self.state.workspace.traces {
            if !trace.settings.visible
                || !preview.group.members.contains(&trace.id)
                || trace
                    .settings
                    .acquisition_for(&range, list.as_deref())
                    .ok()
                    .as_ref()
                    != Some(&preview.group.settings)
                || !trace.settings.display.accepts(&preview.data)
                || trace
                    .last_completed_cycle
                    .is_some_and(|last| preview.cycle_id <= last)
                || trace
                    .preview
                    .as_ref()
                    .is_some_and(|last| preview.cycle_id < last.cycle_id)
            {
                continue;
            }
            trace.preview = Some(preview.clone());
        }
    }

    fn apply_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Connected(info) => {
                self.connection_open = false;
                self.state.desktop.lookup.ports.cancel();
                info!("connected, serial {}", info.serial);
                self.state.connection = ConnectionState::Connected;
                self.state.source.connected();
                self.state.health = Default::default();
                self.state.device_info = Some(info);
                self.state.status_message = Some(StatusMessage::Text(Text::Connected));
                self.state.send(crate::state::WorkerCommand::RefreshStatus);
            }
            WorkerEvent::Disconnected => {
                self.clear_connection();
                self.state.status_message = Some(StatusMessage::Text(Text::Disconnected));
            }
            WorkerEvent::ConnectionLost(message) => {
                if self.state.calibration.busy() {
                    self.state.calibration.error = Some(message.clone());
                }
                self.clear_connection();
                self.state.connection = ConnectionState::Error(message.clone());
                self.state.status_message = Some(message.into());
            }
            WorkerEvent::Error(message) => {
                self.state.source.lost();
                if self.state.calibration.open && self.state.calibration.frozen.is_some() {
                    self.state.calibration.error = Some(message.clone());
                }
                if self.state.calibration.pending.is_some() {
                    self.state.calibration.lost();
                }
                if self.state.connection == ConnectionState::Connecting {
                    self.state.connection = ConnectionState::Error(message.clone());
                }
                self.state.sweep = SweepState::Idle;
                self.state.active_plan = None;
                self.state.run_progress = None;
                self.state.clear_preview();
                self.state.status_message = Some(message.into());
            }
            WorkerEvent::SweepStopped => {
                if self.state.sweep == SweepState::Stopping {
                    self.state.sweep = SweepState::Idle;
                    self.state.run_progress = None;
                    self.state.clear_preview();
                }
            }
            WorkerEvent::SweepTrace(_) => {}
            WorkerEvent::SourceReport(report) => {
                if self.state.connection == ConnectionState::Connected {
                    self.state.source.accept(report, self.state.function);
                }
            }
            WorkerEvent::CalibrationReport(report) => {
                if self.state.connection == ConnectionState::Connected {
                    self.state.calibration.accept(report);
                }
            }
            WorkerEvent::RunProgress(progress) => {
                if self.state.connection == ConnectionState::Connected && self.state.any_running() {
                    if let crate::run_settings::RunProgress::Saved { path, pass_id } = progress {
                        if pass_id != 0
                            && self
                                .state
                                .last_recording
                                .as_ref()
                                .is_none_or(|(last, _)| pass_id > *last)
                        {
                            self.state.last_recording = Some((pass_id, path));
                        }
                    } else {
                        self.state.run_progress = Some(progress);
                    }
                }
            }
            WorkerEvent::Status(snapshot) => {
                if self.state.connection == ConnectionState::Connected {
                    self.state.health.succeed(snapshot, Instant::now());
                }
            }
            WorkerEvent::StatusFailed(message) => {
                if self.state.connection == ConnectionState::Connected {
                    self.state.health.fail(message, Instant::now());
                }
            }
        }
    }

    fn clear_connection(&mut self) {
        self.state.source.lost();
        self.state.calibration.lost();
        self.state.connection = ConnectionState::Disconnected;
        self.state.device_info = None;
        self.state.health = Default::default();
        self.state.sweep = SweepState::Idle;
        self.state.active_plan = None;
        self.state.run_progress = None;
        self.state.clear_preview();
        self.state.acquisition_cancel.cancel();
        self.state.session_cancel.cancel();
    }

    fn poll_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) && !self.closing {
            self.closing = true;
            self.state.export.cancel();
            self.state.workspace.frequency_editor.cancel();
            self.state.workspace.run_editor.cancel();
            self.state.desktop.lookup.cancel();
            self.state.folder_opener.cancel();
            self.state.send(crate::state::WorkerCommand::Shutdown);
        }
        if !self.closing {
            return;
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(20));
            return;
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            warn!("device worker panicked during shutdown");
        }
        self.state.cmd_tx = None;
        if self.state.export.is_pending()
            || self.state.workspace.frequency_editor.is_pending()
            || self.state.workspace.run_editor.is_pending()
            || self.state.desktop.lookup.is_pending()
            || self.state.folder_opener.is_pending()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    /// Save the config after a quiet period, so bursts of edits (dragging
    /// a frequency field) produce at most one write.
    fn persist_config_debounced(&mut self) {
        let current = AppConfig::from_state(&self.state);
        if current == self.last_saved {
            self.dirty_since = None;
            return;
        }
        let since = self.dirty_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= CONFIG_SAVE_DELAY {
            self.persist_config();
        }
    }

    /// Write the current config to disk immediately.
    fn persist_config(&mut self) {
        let current = AppConfig::from_state(&self.state);
        if let Err(e) = crate::config::save(&current) {
            warn!("failed to save config: {e}");
        } else {
            self.last_saved = current;
        }
        self.dirty_since = None;
    }
}

/// Build plot series for an S11 cartesian display from the `z`/`ma`/
/// `loss`/`vswr` columns (protocol doc 4.4).
fn cartesian_series(
    display: S11Display,
    trace: Option<&kcsdi_core::data::SweepData>,
    impedance_visible: [bool; 3],
    language: Language,
) -> Vec<widgets::plot::Series<'static>> {
    let Some(trace) = trace else {
        return Vec::new();
    };
    if trace.mode != kcsdi_core::protocol::StreamMode::S11
        || trace.format != display.wire_format().as_str()
    {
        return Vec::new();
    }
    let column = |name: &'static str, i: usize, color: egui::Color32| widgets::plot::Series {
        name,
        color,
        visible: display != S11Display::Impedance || impedance_visible[i],
        points: trace
            .points
            .iter()
            .map(|p| (p.freq_hz, p.values.get(i).copied().unwrap_or(f64::NAN)))
            .collect(),
    };
    match display {
        S11Display::Phase => vec![column(
            language.text(Text::Phase),
            1,
            theme::TRACE_COLORS[0],
        )],
        S11Display::ReturnLoss => vec![column(
            language.text(Text::ReturnLoss),
            0,
            theme::TRACE_COLORS[0],
        )],
        S11Display::Vswr => vec![column(language.text(Text::Vswr), 0, theme::TRACE_COLORS[0])],
        S11Display::Impedance => vec![
            column("|Z|", 0, theme::TRACE_COLORS[0]),
            column("R", 1, theme::TRACE_COLORS[1]),
            column("X", 2, theme::TRACE_COLORS[2]),
        ],
        S11Display::Magnitude => vec![column("|Z|", 0, theme::TRACE_COLORS[0])],
        S11Display::Resistance => vec![column("R", 1, theme::TRACE_COLORS[0])],
        S11Display::Reactance => vec![column("X", 2, theme::TRACE_COLORS[0])],
        S11Display::Smith => Vec::new(),
    }
}

impl eframe::App for KcsdiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        i18n::set_language(&ctx, self.state.language);
        theme::apply(&ctx, self.state.desktop.settings.theme);
        self.state.export.poll();
        self.state.workspace.frequency_editor.poll();
        self.state.workspace.run_editor.poll();
        self.state.desktop.lookup.poll();
        if let Some(Err(error)) = self.state.folder_opener.poll() {
            self.state.status_message = Some(error.into());
        }
        if self.state.desktop.lookup.is_pending() || self.state.folder_opener.is_pending() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        self.poll_close(&ctx);

        // Drain all pending events from the device worker.
        loop {
            match self.evt_rx.try_recv() {
                Ok(evt) => self.apply_worker_event(evt),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.worker.is_some() && !self.closing {
                        self.state.worker_stopped();
                    }
                    break;
                }
            }
        }
        if let Some(preview) = self.state.preview_mailbox.take() {
            self.apply_preview(preview);
        }
        if !self.closing {
            self.state.refresh_health_if_due(Instant::now());
        }
        if self.closing {
            ui.disable();
            egui::Window::new(self.state.language.text(Text::Closing))
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.spinner();
                    ui.label(self.state.language.text(
                        if self.state.workspace.frequency_editor.is_pending() {
                            Text::FrequencyFilePending
                        } else if self.state.workspace.run_editor.is_pending() {
                            Text::RunFilePending
                        } else if self.state.export.is_pending() {
                            Text::ExportCancelHelp
                        } else {
                            Text::Closing
                        },
                    ));
                });
        }

        if ctx.input(|input| input.key_pressed(egui::Key::F11)) {
            let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if self.state.desktop.page == Page::Instrument {
            self.instrument_ui(ui);
        } else {
            self.desktop_ui(ui);
        }

        let was_connection_open = self.connection_open;
        egui::Window::new(self.state.language.text(Text::Connection))
            .open(&mut self.connection_open)
            .resizable(false)
            .collapsible(false)
            .show(&ctx, |ui| panels::top_bar::show(ui, &mut self.state));
        if was_connection_open && !self.connection_open {
            self.state.desktop.lookup.ports.cancel();
        }

        trace_editor(&ctx, &mut self.state);
        let allowed = self.state.workspace.visible_range();
        if let Some(list) =
            self.state
                .workspace
                .frequency_editor
                .show(&ctx, self.state.language, allowed)
            && !self.closing
        {
            self.state.workspace.frequencies_hz = list;
            self.state.workspace.list_mode = true;
            self.state.workspace.reset_frequency_view();
        }
        if let Some(run) = self
            .state
            .workspace
            .run_editor
            .show(&ctx, self.state.language)
            && !self.closing
        {
            self.state.workspace.run = run;
        }
        if let Some(path) = self.state.workspace.run_editor.take_open_directory()
            && !self.closing
        {
            self.state.folder_opener.request(path, &ctx);
        }
        self.state.reconcile_plan();
        if !self.closing {
            crate::calibration_panel::show(&ctx, &mut self.state);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        self.persist_config_debounced();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_config();
        self.state.desktop.lookup.cancel();
        self.state.folder_opener.cancel();
        self.state.send(crate::state::WorkerCommand::Shutdown);
        self.state.cmd_tx = None;
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            warn!("device worker panicked during exit");
        }
    }
}

impl Drop for KcsdiApp {
    fn drop(&mut self) {
        self.state.desktop.lookup.cancel();
        self.state.folder_opener.cancel();
        self.state.worker_shutdown.cancel();
        self.state.session_cancel.cancel();
        self.state.acquisition_cancel.cancel();
        self.state.cmd_tx = None;
    }
}

impl KcsdiApp {
    fn desktop_ui(&mut self, ui: &mut egui::Ui) {
        if self.state.connection != ConnectionState::Disconnected {
            egui::Panel::bottom("desktop_session_status").show(ui, |ui| {
                if panels::status_bar::show(ui, &mut self.state) {
                    self.connection_open = true;
                }
            });
        }
        desktop::show_home(ui, &mut self.state);
    }

    fn instrument_ui(&mut self, ui: &mut egui::Ui) {
        let language = self.state.language;
        egui::Panel::top("instrument_menu")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(4),
            )
            .show(ui, |ui| {
                ui.visuals_mut().button_frame = false;
                ui.spacing_mut().interact_size.y = 18.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if ui.button(language.text(Text::Devices)).clicked() {
                        self.state.desktop.page = Page::Devices;
                    }
                    for function in crate::source_panel::InstrumentFunction::ALL {
                        if ui
                            .selectable_label(
                                self.state.function == function,
                                function.label(language),
                            )
                            .clicked()
                        {
                            self.state.select_function(function);
                        }
                    }
                    if ui
                        .add_enabled(
                            !self.state.source.busy(),
                            egui::Button::new(language.text(Text::CalibrationWizard)),
                        )
                        .on_disabled_hover_text(language.text(Text::CalibrationStopSource))
                        .clicked()
                    {
                        self.state.workspace.editor = None;
                        self.state.workspace.frequency_editor.cancel();
                        self.state.workspace.run_editor.cancel();
                        self.state.calibration.open();
                    }
                    if ui
                        .add_enabled(
                            self.state.function
                                == crate::source_panel::InstrumentFunction::Measurements
                                && self.state.workspace.traces.len()
                                    < crate::acquisition::MAX_TRACES,
                            egui::Button::new(language.text(Text::AddTrace)),
                        )
                        .clicked()
                    {
                        self.state.workspace.open_add_editor();
                    }
                    if ui.button(language.text(Text::Settings)).clicked() {
                        self.state.desktop.page = Page::Settings;
                    }
                    if ui.button(language.text(Text::About)).clicked() {
                        self.state.desktop.page = Page::About;
                    }
                });
            });
        egui::Panel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(4),
            )
            .show(ui, |ui| {
                if panels::status_bar::show(ui, &mut self.state) {
                    self.connection_open = true;
                }
            });
        if self.state.function.kind().is_some() {
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(16),
                )
                .show(ui, |ui| crate::source_panel::show(ui, &mut self.state));
            return;
        }
        let panel_frame = egui::Frame::new()
            .fill(ui.visuals().panel_fill)
            .inner_margin(8);
        egui::Panel::left("trace_panel")
            .exact_size(256.0)
            .frame(panel_frame)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("frequency_scroll")
                    .show(ui, |ui| {
                        trace_list(ui, &mut self.state);
                        ui.add_space(8.0);
                        panels::workspace_panel::show_sweep(ui, &mut self.state);
                    });
            });
        parameter_panel(ui, &mut self.state);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui.visuals().panel_fill))
            .show(ui, |ui| self.plot_ui(ui));
    }

    fn plot_ui(&mut self, ui: &mut egui::Ui) {
        let cartesian = self
            .state
            .workspace
            .traces
            .iter()
            .any(|trace| trace.settings.visible && !trace.settings.display.is_smith());
        let smith = self
            .state
            .workspace
            .traces
            .iter()
            .any(|trace| trace.settings.visible && trace.settings.display.is_smith());
        if cartesian && smith {
            let height = ui.available_height();
            let minimum = 240.0_f32.min(height * 0.5);
            egui::Panel::top("cartesian_region")
                .default_size(height * 0.5)
                .size_range(minimum..=height - minimum)
                .resizable(true)
                .show(ui, |ui| self.cartesian_ui(ui));
            egui::CentralPanel::default().show(ui, |ui| self.smith_ui(ui));
        } else if smith {
            self.smith_ui(ui);
        } else {
            self.cartesian_ui(ui);
        }
    }

    fn cartesian_ui(&mut self, ui: &mut egui::Ui) {
        let language = self.state.language;
        let workspace = &mut self.state.workspace;
        workspace.ensure_log_x_view();
        let common_x = (workspace.x_view.x_min, workspace.x_view.x_max);
        let mut prepared = Vec::new();
        for trace in workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && !trace.settings.display.is_smith())
        {
            trace.analysis.set_language(language);
            let complete = trace
                .completed
                .clone()
                .filter(|snapshot| trace.settings.display.accepts(&snapshot.data));
            let overlays = if trace.overlays_compatible() {
                trace
                    .analysis
                    .overlay_series(trace.settings.display.columns())
            } else {
                Vec::new()
            };
            let marker_frequencies = if trace.overlays_compatible() {
                complete
                    .as_ref()
                    .map(|complete| trace.analysis.marker_frequencies(&complete.data))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let completed_options = complete.map(|complete| {
                let mut series = trace_series(
                    &trace.settings,
                    Some(&complete.data),
                    language,
                    ui.visuals().dark_mode,
                );
                series.extend(overlays.iter().cloned());
                widgets::plot::PlotOptions {
                    y_label: trace.settings.display.unit(),
                    log_x: workspace.log_x,
                    series,
                }
            });
            if trace.needs_fit
                && !trace.view_locked
                && let Some(options) = &completed_options
            {
                widgets::plot::fit_view(&mut trace.view, options);
                trace.needs_fit = false;
            }
            // Auto-fit owns only this trace's Y. Every trace uses the common X.
            trace.view.x_min = common_x.0;
            trace.view.x_max = common_x.1;
            let mut series = trace_series(
                &trace.settings,
                trace.display_data(),
                language,
                ui.visuals().dark_mode,
            );
            let base_count = series.len();
            series.extend(overlays);
            prepared.push((
                base_count,
                marker_frequencies,
                widgets::plot::PlotOptions {
                    y_label: trace.settings.display.unit(),
                    log_x: workspace.log_x,
                    series,
                },
                completed_options,
            ));
        }
        let mut layers: Vec<_> = workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && !trace.settings.display.is_smith())
            .zip(prepared.iter_mut())
            .map(
                |(trace, (_, marker_frequencies, options, _))| widgets::plot::CartesianLayer {
                    id: trace.id.0,
                    label: format!("T{} {}", trace.id.0, trace.settings.display.label(language)),
                    view: &mut trace.view,
                    options: options.clone(),
                    line_width: trace.settings.line_width,
                    markers: trace.analysis.markers_mut(),
                    marker_frequencies,
                },
            )
            .collect();
        let changes = widgets::plot::show_multi(ui, &mut layers, workspace.selected.map(|id| id.0));
        if let Some(layer) = layers.first() {
            workspace.x_view.x_min = layer.view.x_min;
            workspace.x_view.x_max = layer.view.x_max;
        }
        let options: Vec<_> = layers.iter().map(|layer| layer.options.clone()).collect();
        drop(layers);
        if let Some(id) = changes.activated_trace {
            workspace.selected = Some(crate::acquisition::TraceId(id));
        }
        let mut reset_x = None;
        let bounds = workspace.frequency_bounds();
        for ((trace, (base_count, _, _, completed_options)), options) in workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && !trace.settings.display.is_smith())
            .zip(prepared)
            .zip(options)
        {
            if let Some((_, lock)) = changes.view_locks.iter().find(|(id, _)| *id == trace.id.0) {
                match lock {
                    widgets::plot::ViewLock::Locked => trace.view_locked = true,
                    widgets::plot::ViewLock::Unlocked => {
                        trace.view_locked = false;
                        let (low, high) = trace.settings.display.default_y();
                        trace.view.reset(bounds.0, bounds.1, low, high);
                        if let Some(options) = &completed_options {
                            widgets::plot::fit_view(&mut trace.view, options);
                        }
                        trace.needs_fit = false;
                        reset_x = Some((trace.view.x_min, trace.view.x_max));
                    }
                    widgets::plot::ViewLock::Unchanged => {}
                }
            }
            if trace.settings.display == TraceDisplay::S11(S11Display::Impedance) && base_count == 3
            {
                let visibility = std::array::from_fn(|i| options.series[i].visible);
                if trace.settings.impedance_visible != visibility {
                    trace.settings.impedance_visible = visibility;
                    trace.needs_fit = true;
                }
            } else if base_count == 1 {
                trace.settings.visible = options.series[0].visible;
            }
            trace
                .analysis
                .apply_overlay_visibility(&options.series[base_count..]);
        }
        if let Some((low, high)) = reset_x {
            workspace.x_view.x_min = low;
            workspace.x_view.x_max = high;
            for trace in &mut workspace.traces {
                trace.view.x_min = low;
                trace.view.x_max = high;
            }
            ui.ctx().request_repaint();
        }
    }

    fn smith_ui(&mut self, ui: &mut egui::Ui) {
        let workspace = &mut self.state.workspace;
        let prepared: Vec<_> = workspace
            .traces
            .iter()
            .filter(|trace| trace.settings.visible && trace.settings.display.is_smith())
            .map(|trace| {
                (
                    trace.display_data().cloned(),
                    trace
                        .overlays_compatible()
                        .then(|| trace.analysis.held_trace())
                        .flatten(),
                    trace
                        .overlays_compatible()
                        .then(|| trace.analysis.marker_trace())
                        .flatten(),
                )
            })
            .collect();
        let mut layers: Vec<_> = workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && trace.settings.display.is_smith())
            .zip(prepared.iter())
            .map(
                |(trace, (data, held, marker_data))| widgets::smith::SmithLayer {
                    id: trace.id.0,
                    label: format!("T{}", trace.id.0),
                    trace: data.as_ref(),
                    held: held.as_ref(),
                    marker_trace: marker_data.as_ref(),
                    color: displayed_color(trace.settings.color, ui.visuals().dark_mode),
                    line_width: trace.settings.line_width,
                    markers: trace.analysis.markers_mut(),
                },
            )
            .collect();
        layers
            .sort_by_key(|layer| Some(crate::acquisition::TraceId(layer.id)) == workspace.selected);
        let activated = widgets::smith::show_multi(ui, &mut workspace.smith, &mut layers);
        drop(layers);
        if let Some(id) = activated {
            workspace.selected = Some(crate::acquisition::TraceId(id));
        }
    }
}

fn trace_series(
    settings: &TraceSettings,
    data: Option<&kcsdi_core::data::SweepData>,
    language: Language,
    dark: bool,
) -> Vec<widgets::plot::Series<'static>> {
    let Some(data) = data.filter(|data| settings.display.accepts(data)) else {
        return Vec::new();
    };
    let mut series = match settings.display {
        TraceDisplay::Spec => vec![widgets::plot::Series {
            name: language.text(Text::Level),
            color: settings.color,
            visible: true,
            points: data
                .points
                .iter()
                .map(|point| {
                    (
                        point.freq_hz,
                        point.values.first().copied().unwrap_or(f64::NAN),
                    )
                })
                .collect(),
        }],
        TraceDisplay::S11(display) => {
            cartesian_series(display, Some(data), settings.impedance_visible, language)
        }
        TraceDisplay::S21(display) => vec![widgets::plot::Series {
            name: display.label(language),
            color: settings.color,
            visible: true,
            points: data
                .points
                .iter()
                .map(|point| {
                    (
                        point.freq_hz,
                        point
                            .values
                            .get(settings.display.columns()[0])
                            .copied()
                            .unwrap_or(f64::NAN),
                    )
                })
                .collect(),
        }],
    };
    if let Some(first) = series.first_mut() {
        first.color = settings.color;
    }
    for curve in &mut series {
        curve.color = displayed_color(curve.color, dark);
    }
    series
}

fn displayed_color(color: egui::Color32, dark: bool) -> egui::Color32 {
    theme::TRACE_COLORS
        .iter()
        .position(|candidate| *candidate == color)
        .map_or(color, |index| theme::trace_colors(dark)[index])
}

pub(crate) fn parameter_panel(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    egui::Panel::right("params_panel")
        .exact_size(256.0)
        .frame(
            egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .inner_margin(8),
        )
        .show(ui, |ui| {
            panels::workspace_panel::run_button(ui, state);
            egui::Panel::bottom("trace_export")
                .default_size(148.0)
                .min_size(148.0)
                .show_separator_line(true)
                .show(ui, |ui| {
                    crate::export::show(ui, &mut state.export, &state.workspace, language)
                });
            egui::ScrollArea::vertical()
                .id_salt("analysis_scroll")
                .show(ui, |ui| {
                    let editable = state.sweep != SweepState::Stopping
                        && state.connection != ConnectionState::Disconnecting;
                    let list_mode = state.workspace.list_mode;
                    let (start, stop) = state.workspace.frequency_bounds();
                    let sweep_center = (start + stop) / 2.0;
                    let mut marker_center = None;
                    if let Some(trace) = state.workspace.selected_mut() {
                        ui.push_id(trace.id.0, |ui| {
                            ui.label(format!(
                                "T{} {}",
                                trace.id.0,
                                trace.settings.display.label(language)
                            ));
                            let mut settings = trace.settings.clone();
                            ui.add_enabled_ui(editable, |ui| {
                                panels::workspace_panel::format_fields(ui, &mut settings, language);
                                panels::workspace_panel::receiver_fields(
                                    ui,
                                    &mut settings,
                                    language,
                                );
                            });
                            trace.update_settings(settings);
                            let complete = trace
                                .completed
                                .clone()
                                .filter(|snapshot| trace.settings.display.accepts(&snapshot.data));
                            let columns = trace.settings.display.columns();
                            trace
                                .analysis
                                .set_column((columns.len() == 1).then(|| columns[0]));
                            ui.add_enabled_ui(editable, |ui| {
                                marker_center = trace.analysis.controls_for_display(
                                    ui,
                                    language,
                                    complete.as_deref(),
                                    trace.settings.display.is_smith(),
                                    sweep_center,
                                    !list_mode,
                                );
                            });
                            if trace.completed.is_some() && complete.is_none() {
                                ui.label(language.text(Text::RunForDisplay));
                            }
                            panels::workspace_panel::display_fields(ui, trace, language);
                        });
                    }
                    if let Some(frequency) = marker_center
                        && let Err(error) = state.workspace.center_on_marker(frequency)
                    {
                        state.status_message = Some(error.to_string().into());
                    }
                    ui.label(language.text(Text::Display));
                    if widgets::plot::log_x_control(ui, &mut state.workspace.log_x) {
                        ui.ctx().request_repaint();
                    }
                });
        });
}

fn trace_list(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(language.text(Text::TraceList)).monospace())
            .on_hover_text(language.text(Text::VisibleTracesRun));
        if ui
            .add_enabled(
                state.workspace.traces.len() < crate::acquisition::MAX_TRACES,
                egui::Button::new("+"),
            )
            .on_hover_text(language.text(
                if state.workspace.traces.len() < crate::acquisition::MAX_TRACES {
                    Text::AddTrace
                } else {
                    Text::TraceLimit
                },
            ))
            .clicked()
        {
            state.workspace.open_add_editor();
        }
    });
    if state.workspace.traces.is_empty() {
        ui.label(language.text(Text::NoTraces));
    }
    let mut edit = None;
    let mut delete = None;
    for trace in &mut state.workspace.traces {
        let selected = state.workspace.selected == Some(trace.id);
        let fill = if selected {
            ui.visuals().faint_bg_color
        } else {
            ui.visuals().panel_fill
        };
        let card = ui
            .push_id(("trace_card", trace.id.0), |ui| {
                ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                    egui::Frame::new()
                        .inner_margin(8)
                        .fill(fill)
                        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                        .corner_radius(4)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(format!("T{}", trace.id.0))
                                                .strong(),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    state.workspace.selected = Some(trace.id);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.menu_button("...", |ui| {
                                            if ui.button(language.text(Text::Edit)).clicked() {
                                                edit = Some(trace.id);
                                                ui.close();
                                            }
                                            if ui.button(language.text(Text::Delete)).clicked() {
                                                delete = Some(trace.id);
                                                ui.close();
                                            }
                                        });
                                        visibility_control(
                                            ui,
                                            &mut trace.settings.visible,
                                            language,
                                        );
                                    },
                                );
                            });
                            if ui
                                .add(
                                    egui::Button::new(trace.settings.display.label(language))
                                        .frame(false),
                                )
                                .clicked()
                            {
                                state.workspace.selected = Some(trace.id);
                            }
                        });
                })
            })
            .inner;
        if card.response.clicked() {
            state.workspace.selected = Some(trace.id);
        }
        if selected {
            ui.painter().line_segment(
                [
                    card.response.rect.left_top() + egui::vec2(1.0, 2.0),
                    card.response.rect.left_bottom() + egui::vec2(1.0, -2.0),
                ],
                egui::Stroke::new(3.0, trace.settings.color),
            );
        }
        ui.add_space(4.0);
    }
    if let Some(id) = delete {
        state.workspace.remove_trace(id);
    }
    if let Some(id) = edit
        && let Some(trace) = state.workspace.traces.iter().find(|trace| trace.id == id)
    {
        state.workspace.editor = Some(TraceEditor {
            id: Some(id),
            settings: trace.settings.clone(),
        });
    }
}

fn trace_editor(ctx: &egui::Context, state: &mut AppState) {
    let Some(mut editor) = state.workspace.editor.take() else {
        return;
    };
    let language = state.language;
    let mut open = true;
    let mut save = false;
    let mut cancel = false;
    egui::Window::new(language.text(if editor.id.is_some() {
        Text::TraceSettings
    } else {
        Text::AddTrace
    }))
    .id(egui::Id::new("trace_editor"))
    .open(&mut open)
    .resizable(false)
    .collapsible(false)
    .show(ctx, |ui| {
        ui.add_enabled_ui(
            state.sweep != SweepState::Stopping
                && state.connection != ConnectionState::Disconnecting,
            |ui| {
                panels::workspace_panel::settings_fields(ui, &mut editor.settings, language);
                ui.horizontal(|ui| {
                    save = ui.button(language.text(Text::Save)).clicked();
                    cancel = ui.button(language.text(Text::Cancel)).clicked();
                });
            },
        );
    });
    if save {
        if let Some(id) = editor.id {
            if let Some(trace) = state
                .workspace
                .traces
                .iter_mut()
                .find(|trace| trace.id == id)
            {
                trace.update_settings(editor.settings);
            }
        } else if let Err(error) = state.workspace.add_trace(editor.settings) {
            state.status_message = Some(error.to_string().into());
        }
    } else if open && !cancel {
        state.workspace.editor = Some(editor);
    }
}

fn visibility_control(ui: &mut egui::Ui, visible: &mut bool, language: Language) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    if response.clicked() {
        *visible = !*visible;
    }
    let color = if *visible {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let center = rect.center();
    ui.painter().add(egui::Shape::ellipse_stroke(
        center,
        egui::vec2(8.0, 5.0),
        egui::Stroke::new(1.2, color),
    ));
    ui.painter().circle_filled(center, 2.0, color);
    if !*visible {
        ui.painter().line_segment(
            [
                center + egui::vec2(-7.0, 7.0),
                center + egui::vec2(7.0, -7.0),
            ],
            egui::Stroke::new(1.2, color),
        );
    }
    response.on_hover_text(language.text(if *visible { Text::Hide } else { Text::Show }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::{CompletedSweep, TraceId};
    use crate::state::{S21Display, WorkerCommand};
    use crate::workspace::{SweepRange, Workspace};
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::protocol::StreamMode;
    use std::time::SystemTime;

    fn test_app(state: AppState) -> KcsdiApp {
        let (_, evt_rx) = mpsc::channel();
        KcsdiApp {
            last_saved: AppConfig::from_state(&state),
            state,
            evt_rx,
            dirty_since: None,
            connection_open: false,
            worker: None,
            closing: false,
        }
    }

    fn completed_impedance() -> SweepData {
        SweepData {
            mode: StreamMode::S11,
            format: "z".into(),
            points: (0..3)
                .map(|index| SweepPoint {
                    freq_hz: 1_000_000.0 + f64::from(index) * 500_000.0,
                    values: vec![50.0, 50.0, 0.0],
                })
                .collect(),
        }
    }

    fn active_impedance_app() -> KcsdiApp {
        let mut state = AppState {
            connection: ConnectionState::Connected,
            session_id: 1,
            health: crate::health::HealthState {
                snapshot: Some(crate::health::tests::snapshot(42.0, Instant::now())),
                ..Default::default()
            },
            ..Default::default()
        };
        state.workspace = Workspace::empty(SweepRange::new(1_000_000.0, 2_000_000.0, 3));
        state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Impedance),
                ..Default::default()
            })
            .unwrap();
        state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
        let snapshot = Arc::new(CompletedSweep {
            data: completed_impedance(),
            settings: state.active_plan.as_ref().unwrap().groups[0]
                .settings
                .clone(),
            session_id: 1,
            completed_at: SystemTime::UNIX_EPOCH,
        });
        let trace = state.workspace.selected_mut().unwrap();
        trace.analysis.observe(&snapshot);
        trace.completed = Some(snapshot);
        trace.needs_fit = false;
        test_app(state)
    }

    fn complete_data(app: &KcsdiApp) -> Option<SweepData> {
        app.state
            .workspace
            .selected()
            .and_then(|trace| trace.completed.as_ref())
            .map(|snapshot| snapshot.data.clone())
    }

    #[test]
    fn recording_progress_is_request_scoped_and_cannot_restart_stopped_work() {
        use crate::run_settings::RunProgress;
        let mut app = active_impedance_app();
        let event = |session_id, request_id, event| EventEnvelope {
            session_id,
            request_id,
            cycle_id: None,
            event: WorkerEvent::RunProgress(event),
        };
        let session = app.state.session_id;
        let request = app.state.request_id;
        app.apply_worker_event(event(session, request, RunProgress::Saving));
        assert!(matches!(app.state.run_progress, Some(RunProgress::Saving)));
        assert!(app.state.any_running());
        let path = std::env::temp_dir().join("recording-test.csv");
        app.apply_worker_event(event(
            session,
            request,
            RunProgress::Saved {
                path: path.clone(),
                pass_id: 2,
            },
        ));
        app.apply_worker_event(event(
            session,
            request,
            RunProgress::Saved {
                path: path.with_extension("old"),
                pass_id: 1,
            },
        ));
        assert_eq!(app.state.last_recording, Some((2, path.clone())));
        app.state.workspace.run.interval_ms = 1000;
        app.state.reconcile_plan();
        assert!(app.state.request_id > request);
        assert!(app.state.last_recording.is_none());
        app.apply_worker_event(event(
            session,
            request,
            RunProgress::Saved {
                path: path.clone(),
                pass_id: 3,
            },
        ));
        assert!(app.state.last_recording.is_none());
        app.apply_worker_event(event(
            session + 1,
            app.state.request_id,
            RunProgress::Saving,
        ));
        assert!(matches!(
            app.state.run_progress,
            Some(RunProgress::Acquiring)
        ));
        app.state.send(WorkerCommand::StopSweep);
        app.apply_worker_event(event(
            session,
            app.state.request_id,
            RunProgress::Waiting {
                until: Instant::now() + Duration::from_secs(60),
            },
        ));
        assert_eq!(app.state.sweep, SweepState::Stopping);
        assert!(app.state.run_progress.is_none());
        assert!(complete_data(&app).is_some());
    }

    #[test]
    fn source_switches_stop_the_previous_operation_and_reject_stale_reports() {
        use crate::source_panel::{InstrumentFunction, SourcePending};
        use kcsdi_core::source::{SourceKind, SourceOutputState, SourceReport};
        let mut app = active_impedance_app();
        let complete = complete_data(&app).unwrap();
        let sweep_request = app.state.request_id;
        app.state.select_function(InstrumentFunction::RfSource);
        assert_eq!(app.state.sweep, SweepState::Stopping);
        assert!(app.state.active_plan.is_none());
        let emit = |request_id, event| EventEnvelope {
            session_id: 1,
            request_id,
            cycle_id: None,
            event,
        };
        app.apply_worker_event(emit(sweep_request, WorkerEvent::SweepStopped));
        assert_eq!(app.state.sweep, SweepState::Stopping);
        app.apply_worker_event(emit(app.state.request_id, WorkerEvent::SweepStopped));
        assert_eq!(app.state.sweep, SweepState::Idle);
        let params = app.state.source.config.rf.params(SourceKind::Rf).unwrap();
        app.state.send(WorkerCommand::StartSource(params));
        let source_request = app.state.request_id;
        let started = SourceReport {
            state: SourceOutputState::Requested(SourceKind::Rf),
            warning: None,
        };
        app.apply_worker_event(emit(source_request, WorkerEvent::SourceReport(started)));
        assert_eq!(app.state.source.report, started);
        app.state.reconcile_plan();
        assert_eq!(app.state.request_id, source_request);
        assert!(!app.state.any_running());
        app.state.select_function(InstrumentFunction::AfSource);
        let stop_request = app.state.request_id;
        assert!(stop_request > source_request);
        assert_eq!(app.state.source.pending, Some(SourcePending::Stop));
        app.apply_worker_event(emit(source_request, WorkerEvent::SourceReport(started)));
        assert_eq!(app.state.source.pending, Some(SourcePending::Stop));
        app.apply_worker_event(emit(
            stop_request,
            WorkerEvent::SourceReport(SourceReport {
                state: SourceOutputState::StopSent,
                warning: None,
            }),
        ));
        assert!(!app.state.source.busy());
        assert!(app.state.source.requested.is_none());
        assert_eq!(app.state.function, InstrumentFunction::AfSource);
        assert_eq!(complete_data(&app).unwrap(), complete);
        assert!(app.state.active_plan.is_none());
    }

    #[test]
    fn unknown_source_blocks_measurements_and_health_after_reconnect() {
        use crate::source_panel::InstrumentFunction;
        use kcsdi_core::source::SourceOutputState;
        let mut app = active_impedance_app();
        app.state.sweep = SweepState::Idle;
        app.state.active_plan = None;
        app.state.source.report.state = SourceOutputState::Unknown;
        app.clear_connection();
        app.state.connection = ConnectionState::Connected;
        app.state.source.connected();
        app.state.function = InstrumentFunction::Measurements;
        let request = app.state.request_id;
        app.state.send(WorkerCommand::RunWorkspace(
            app.state.workspace.plan().unwrap(),
        ));
        app.state.send(WorkerCommand::RefreshStatus);
        assert_eq!(app.state.request_id, request);
        assert!(app.state.source.busy());
        assert!(!app.state.health.pending);
        assert!(!app.state.any_running());
    }

    #[test]
    fn full_source_pages_fit_both_languages_at_both_window_sizes() {
        use crate::source_panel::InstrumentFunction;
        use kcsdi_core::source::SourceKind;
        for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
            for language in Language::ALL {
                for mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                    for function in [InstrumentFunction::RfSource, InstrumentFunction::AfSource] {
                        let ctx = egui::Context::default();
                        theme::setup(&ctx);
                        theme::apply(&ctx, mode);
                        let mut app = active_impedance_app();
                        app.state.language = language;
                        app.state.function = function;
                        app.state.sweep = SweepState::Idle;
                        app.state.active_plan = None;
                        let kind = function.kind().unwrap();
                        match kind {
                            SourceKind::Rf => {
                                app.state.source.config.rf.frequency_hz = kind.max_frequency_hz();
                                app.state.source.config.rf.modulation = "ask".into();
                            }
                            SourceKind::Af => {
                                app.state.source.config.af.frequency_hz = kind.max_frequency_hz();
                                app.state.source.config.af.amplitude_mv = 3_000;
                                app.state.source.config.af.modulation = "pm".into();
                                app.state.source.config.af.pm_phase_deg = -180;
                            }
                        }
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        for _ in 0..3 {
                            let mut output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| app.instrument_ui(ui),
                            );
                            let shapes = std::mem::take(&mut output.shapes);
                            output.drop_without_applying_deltas();
                            for key in [
                                Text::SourceStart,
                                Text::SourcePort,
                                Text::SourceModulation,
                                Text::SourceModFrequency,
                                Text::SourceAmplitude,
                            ] {
                                let shape = shapes.iter().find(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == language.text(key))).expect("source page label visible");
                                let egui::Shape::Text(text) = &shape.shape else {
                                    unreachable!()
                                };
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    screen.contains_rect(bounds)
                                        && shape.clip_rect.contains_rect(bounds),
                                    "{function:?} {language:?} {key:?}: {bounds:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn calibration_retains_complete_exports_and_rejects_stale_step_reports() {
        use kcsdi_core::calibration::{
            CalibrationKind, CalibrationParams, CalibrationPhase, CalibrationPrompt,
            CalibrationReport,
        };
        let mut app = active_impedance_app();
        let completed = app
            .state
            .workspace
            .selected()
            .unwrap()
            .completed
            .clone()
            .unwrap();
        for display in [TraceDisplay::S11(S11Display::Impedance), TraceDisplay::Spec] {
            let id = app
                .state
                .workspace
                .add_trace(TraceSettings {
                    display,
                    visible: false,
                    ..Default::default()
                })
                .unwrap();
            let trace = app
                .state
                .workspace
                .traces
                .iter_mut()
                .find(|trace| trace.id == id)
                .unwrap();
            trace.completed = Some(completed.clone());
        }
        for trace in &mut app.state.workspace.traces {
            trace
                .analysis
                .restore_config(&crate::analysis_tools::AnalysisConfig {
                    hold: true,
                    max_hold: true,
                    ..Default::default()
                });
            trace.analysis.observe(&completed);
            assert!(trace.analysis.held_trace().is_some());
        }
        let previous_request = app.state.request_id;
        app.state.send(WorkerCommand::StartCalibration(
            CalibrationParams::S11System,
        ));
        assert!(!app.state.any_running());
        assert!(app.state.active_plan.is_none());
        assert!(app.state.calibration.busy());
        for trace in &app.state.workspace.traces {
            assert!(Arc::ptr_eq(trace.completed.as_ref().unwrap(), &completed));
            assert!(trace.analysis.held_trace().is_none());
        }
        let current_request = app.state.request_id;
        let session_id = app.state.session_id;
        let event = |request_id, phase| EventEnvelope {
            session_id,
            request_id,
            cycle_id: None,
            event: WorkerEvent::CalibrationReport(CalibrationReport {
                kind: Some(CalibrationKind::S11System),
                phase,
            }),
        };
        app.apply_worker_event(event(previous_request, CalibrationPhase::Completed));
        assert_eq!(
            app.state.calibration.report.phase,
            CalibrationPhase::NotStarted
        );
        app.apply_worker_event(event(
            current_request,
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
        ));
        assert_eq!(
            app.state.calibration.current_prompt,
            Some(CalibrationPrompt::Short)
        );
        app.state
            .send(WorkerCommand::AdvanceCalibration(CalibrationPrompt::Short));
        app.apply_worker_event(event(
            current_request,
            CalibrationPhase::Prompt(CalibrationPrompt::Open),
        ));
        assert_eq!(
            app.state.calibration.current_prompt,
            Some(CalibrationPrompt::Short)
        );
        app.state.send(WorkerCommand::CancelCalibration);
        let cancel_request = app.state.request_id;
        app.apply_worker_event(event(cancel_request, CalibrationPhase::Cancelled));
        assert!(!app.state.calibration.busy());
        assert!(Arc::ptr_eq(
            app.state
                .workspace
                .selected()
                .unwrap()
                .completed
                .as_ref()
                .unwrap(),
            &completed
        ));
    }

    #[test]
    fn full_calibration_modal_fits_languages_themes_and_packet_phases() {
        use kcsdi_core::calibration::{
            CalibrationParams, CalibrationPhase, CalibrationPrompt, CalibrationReport,
            UserCalibrationParams,
        };
        let user = UserCalibrationParams::from_range(
            1_000_000,
            2_000_001,
            1001,
            kcsdi_core::model::Rbw::R30k,
        )
        .unwrap();
        for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
            for language in Language::ALL {
                for mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                    let ctx = egui::Context::default();
                    theme::setup(&ctx);
                    theme::apply(&ctx, mode);
                    for params in [
                        CalibrationParams::S11System,
                        CalibrationParams::S21System,
                        CalibrationParams::S11User(user),
                        CalibrationParams::S21User(user),
                    ] {
                        for phase in [
                            CalibrationPhase::NotStarted,
                            CalibrationPhase::WarmingUp,
                            CalibrationPhase::Measuring,
                            CalibrationPhase::Completed,
                            CalibrationPhase::Unknown,
                        ] {
                            let mut app = active_impedance_app();
                            app.state.language = language;
                            app.state.calibration.begin(params);
                            app.state.calibration.pending = None;
                            app.state.calibration.accept(CalibrationReport {
                                kind: Some(params.kind()),
                                phase: CalibrationPhase::Prompt(
                                    if params.kind().mode() == StreamMode::S11 {
                                        CalibrationPrompt::Open
                                    } else {
                                        CalibrationPrompt::Through
                                    },
                                ),
                            });
                            app.state.calibration.report.phase = phase;
                            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                            for _ in 0..3 {
                                let mut output = ctx.run_ui(
                                    egui::RawInput {
                                        screen_rect: Some(screen),
                                        ..Default::default()
                                    },
                                    |ui| {
                                        app.instrument_ui(ui);
                                        crate::calibration_panel::show(ui.ctx(), &mut app.state);
                                    },
                                );
                                let shapes = std::mem::take(&mut output.shapes);
                                output.drop_without_applying_deltas();
                                let mut keys = vec![Text::CalibrationWizard];
                                if phase == CalibrationPhase::NotStarted {
                                    keys.extend([
                                        Text::CalibrationConsent,
                                        Text::CalibrationStart,
                                        Text::CalibrationWriteWarning,
                                    ]);
                                } else if phase == CalibrationPhase::Completed {
                                    keys.extend([
                                        Text::CalibrationCompleted,
                                        Text::CalibrationReacquire,
                                        Text::Close,
                                    ]);
                                } else if phase == CalibrationPhase::Unknown {
                                    keys.extend([
                                        Text::CalibrationUnknown,
                                        Text::CalibrationChanged,
                                        Text::Close,
                                    ]);
                                } else {
                                    keys.push(Text::Cancel);
                                }
                                for key in keys {
                                    let matched: Vec<_> = shapes.iter().filter(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == language.text(key))).collect();
                                    assert!(
                                        !matched.is_empty(),
                                        "{language:?} {phase:?} missing {key:?}"
                                    );
                                    for shape in matched {
                                        let egui::Shape::Text(text) = &shape.shape else {
                                            unreachable!()
                                        };
                                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                        assert!(
                                            screen.contains_rect(bounds)
                                                && shape.clip_rect.contains_rect(bounds),
                                            "{language:?} {phase:?} {key:?}: {bounds:?}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn calibration_rejection_keeps_authoritative_prompt_and_cancel_available() {
        use kcsdi_core::calibration::{
            CalibrationKind, CalibrationParams, CalibrationPhase, CalibrationPrompt,
            CalibrationReport,
        };
        let mut app = active_impedance_app();
        app.state.send(WorkerCommand::StartCalibration(
            CalibrationParams::S11System,
        ));
        let report = CalibrationReport {
            kind: Some(CalibrationKind::S11System),
            phase: CalibrationPhase::Prompt(CalibrationPrompt::Open),
        };
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: None,
            event: WorkerEvent::CalibrationReport(report),
        });
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: None,
            event: WorkerEvent::Error("Calibration rejected an out-of-date step".into()),
        });
        assert_eq!(app.state.calibration.report, report);
        assert!(app.state.calibration.busy());
        assert!(app.state.calibration.error.is_some());
        app.state.stop_operation();
        assert_eq!(
            app.state.calibration.pending,
            Some(crate::calibration_panel::Pending::Cancel)
        );
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: None,
            event: WorkerEvent::CalibrationReport(report),
        });
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: None,
            event: WorkerEvent::Error("Calibration control request was rejected".into()),
        });
        assert_eq!(app.state.calibration.report, report);
        assert_eq!(app.state.calibration.pending, None);
        assert!(app.state.calibration.busy());
    }

    #[test]
    fn recording_error_preserves_connection_and_complete_measurements() {
        let mut app = active_impedance_app();
        let data = complete_data(&app);
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: None,
            event: WorkerEvent::Error("Recording failed: disk full".into()),
        });
        assert_eq!(app.state.connection, ConnectionState::Connected);
        assert_eq!(app.state.sweep, SweepState::Idle);
        assert!(app.state.run_progress.is_none());
        assert_eq!(complete_data(&app), data);
        assert!(app.state.status_message.is_some());
    }

    #[test]
    fn changing_same_count_list_rejects_old_completions_and_previews() {
        let mut app = active_impedance_app();
        let original = app
            .state
            .workspace
            .selected()
            .unwrap()
            .completed
            .clone()
            .unwrap();
        app.state.workspace.list_mode = true;
        app.state.workspace.frequencies_hz = vec![1_000_000, 1_500_000, 2_000_000];
        app.state.reconcile_plan();
        let stale = completion(&app, 0, 1);
        let old_preview = prefix(&app, 0, 1, 2);
        app.state.workspace.frequencies_hz[1] = 1_750_000;
        // The UI may edit definitions before the end-of-frame replan.
        app.apply_worker_event(stale);
        app.apply_preview(old_preview);
        assert!(Arc::ptr_eq(
            app.state
                .workspace
                .selected()
                .unwrap()
                .completed
                .as_ref()
                .unwrap(),
            &original
        ));
        assert!(app.state.workspace.selected().unwrap().preview.is_none());
        app.state.reconcile_plan();
        app.apply_worker_event(completion(&app, 0, 2));
        let complete = app
            .state
            .workspace
            .selected()
            .unwrap()
            .completed
            .as_ref()
            .unwrap();
        assert_eq!(
            complete.settings,
            app.state.active_plan.as_ref().unwrap().groups[0].settings
        );
        assert!(
            matches!(&complete.settings, crate::acquisition::AcquisitionSettings::List { frequencies_hz, .. } if frequencies_hz[1] == 1_750_000)
        );
    }

    fn prefix(
        app: &KcsdiApp,
        group: usize,
        cycle_id: u64,
        points: usize,
    ) -> crate::preview::PreviewEnvelope {
        let group = app.state.active_plan.as_ref().unwrap().groups[group].clone();
        let mut data = completed_impedance();
        data.mode = group.settings.mode();
        data.format = group.settings.format().into();
        data.points.truncate(points);
        if data.mode == StreamMode::Spec {
            for point in &mut data.points {
                point.values = vec![-20.0];
            }
        } else {
            for point in &mut data.points {
                point.values = match data.format.as_str() {
                    "ma" => vec![0.5, -90.0],
                    "loss" => vec![-3.0],
                    "delay" => vec![-5e-9],
                    _ => point.values.clone(),
                };
            }
        }
        crate::preview::PreviewEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id,
            group,
            data,
        }
    }

    fn completion(app: &KcsdiApp, group: usize, cycle: u64) -> EventEnvelope {
        let preview = prefix(app, group, cycle, 3);
        EventEnvelope {
            session_id: app.state.session_id,
            request_id: app.state.request_id,
            cycle_id: Some(cycle),
            event: WorkerEvent::SweepTrace(SweepDelivery {
                members: preview.group.members,
                snapshot: Arc::new(CompletedSweep {
                    data: preview.data,
                    settings: preview.group.settings,
                    session_id: app.state.session_id,
                    completed_at: SystemTime::UNIX_EPOCH,
                }),
            }),
        }
    }

    fn plot_frame(app: &mut KcsdiApp) {
        egui::Context::default()
            .run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| app.plot_ui(ui),
            )
            .drop_without_applying_deltas();
    }

    fn interaction_frame(
        ctx: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        time: f64,
        contents: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                time: Some(time),
                ..Default::default()
            },
            contents,
        );
        output.textures_delta.clear();
        output
    }

    fn text_bounds(output: &egui::FullOutput, label: &str) -> egui::Rect {
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing text {label}"))
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn trace_card_background_selects_without_stealing_child_controls() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            theme::setup(&ctx);
            let mut app = active_impedance_app();
            app.state.language = language;
            let second = app
                .state
                .workspace
                .add_trace(TraceSettings::default())
                .unwrap();
            app.state.workspace.selected = Some(TraceId(1));
            let request = app.state.request_id;
            let size = egui::vec2(400.0, 400.0);
            let mut time = 0.0;
            let mut frame = |state: &mut AppState, events| {
                time += 0.02;
                interaction_frame(&ctx, size, events, time, |ui| {
                    ui.set_max_width(256.0);
                    trace_list(ui, state);
                })
            };
            let output = frame(&mut app.state, vec![]);
            let heading = text_bounds(&output, "T2").center();
            let card = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(shape)
                        if shape.rect.contains(heading)
                            && shape.rect.width() > 200.0
                            && shape.rect.height() < 110.0 =>
                    {
                        Some(shape.rect)
                    }
                    _ => None,
                })
                .unwrap();
            let body = card.right_bottom() - egui::vec2(12.0, 10.0);
            for events in [
                vec![egui::Event::PointerMoved(body)],
                vec![pointer_button(body, true)],
                vec![pointer_button(body, false)],
            ] {
                frame(&mut app.state, events).drop_without_applying_deltas();
            }
            assert_eq!(app.state.workspace.selected, Some(second));
            assert_eq!(app.state.request_id, request);
            app.state.workspace.selected = Some(TraceId(1));
            let eye = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Ellipse(shape) if card.contains(shape.center) => {
                        Some(shape.center)
                    }
                    _ => None,
                })
                .unwrap();
            let menu = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.text() == "..." && card.contains(text.pos) =>
                    {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap();
            for position in [eye, menu] {
                for events in [
                    vec![egui::Event::PointerMoved(position)],
                    vec![pointer_button(position, true)],
                    vec![pointer_button(position, false)],
                ] {
                    frame(&mut app.state, events).drop_without_applying_deltas();
                }
                assert_eq!(app.state.workspace.selected, Some(TraceId(1)));
            }
            assert!(!app.state.workspace.traces[1].settings.visible);
            assert_eq!(app.state.request_id, request);
            output.drop_without_applying_deltas();
        }
    }

    fn two_marker_traces(display: S11Display) -> KcsdiApp {
        use crate::analysis_tools::{AnalysisConfig, MarkerTarget};
        let mut app = active_impedance_app();
        let settings = TraceSettings {
            display: TraceDisplay::S11(display),
            ..Default::default()
        };
        app.state.workspace.traces[0].update_settings(settings.clone());
        app.state.workspace.add_trace(settings).unwrap();
        app.state.reconcile_plan();
        let mut delivery = completion(&app, 0, 1);
        if let WorkerEvent::SweepTrace(delivery) = &mut delivery.event {
            for (point, resistance) in Arc::make_mut(&mut delivery.snapshot)
                .data
                .points
                .iter_mut()
                .zip([10.0, 50.0, 200.0])
            {
                point.values = vec![resistance, resistance, 0.0];
            }
        }
        app.apply_worker_event(delivery);
        for trace in &mut app.state.workspace.traces {
            trace.analysis.restore_config(&AnalysisConfig {
                markers: vec![widgets::plot::Marker {
                    id: 1,
                    frequency_hz: 1e6,
                    selected: true,
                    ..Default::default()
                }],
                hold: true,
                column: 1,
                target: if display == S11Display::Smith {
                    MarkerTarget::Hold
                } else {
                    MarkerTarget::Current
                },
                ..Default::default()
            });
            trace.analysis.observe(trace.completed.as_ref().unwrap());
        }
        app.state.workspace.selected = Some(TraceId(1));
        app.apply_preview(prefix(&app, 0, 2, 1));
        app
    }

    #[test]
    fn compatible_preview_markers_drag_on_complete_targets_and_select_their_trace() {
        for display in [S11Display::Resistance, S11Display::Smith] {
            let mut app = two_marker_traces(display);
            let ctx = egui::Context::default();
            let size = egui::vec2(800.0, 600.0);
            let request = app.state.request_id;
            let original = app.state.workspace.traces[1].completed.clone().unwrap();
            let output = interaction_frame(&ctx, size, vec![], 0.0, |ui| app.plot_ui(ui));
            let from = text_bounds(&output, "T2 M1").center();
            output.drop_without_applying_deltas();
            let to = if display == S11Display::Smith {
                egui::pos2(700.0, 315.0)
            } else {
                egui::pos2(795.0, from.y + 12.0)
            };
            for (index, events) in [
                vec![egui::Event::PointerMoved(from)],
                vec![pointer_button(from, true)],
                vec![egui::Event::PointerMoved(to)],
                vec![pointer_button(to, false)],
            ]
            .into_iter()
            .enumerate()
            {
                interaction_frame(&ctx, size, events, (index + 1) as f64 * 0.02, |ui| {
                    app.plot_ui(ui)
                })
                .drop_without_applying_deltas();
                if index == 0 {
                    assert_eq!(app.state.workspace.selected, Some(TraceId(1)));
                }
            }
            assert_eq!(app.state.workspace.selected, Some(TraceId(2)));
            assert_eq!(
                app.state.workspace.traces[1].analysis.markers()[0].frequency_hz,
                2e6
            );
            assert_eq!(
                app.state.workspace.traces[0].analysis.markers()[0].frequency_hz,
                1e6
            );
            assert_eq!(app.state.request_id, request);
            assert!(Arc::ptr_eq(
                app.state.workspace.traces[1].completed.as_ref().unwrap(),
                &original
            ));
            assert_eq!(
                app.state.workspace.traces[1]
                    .preview
                    .as_ref()
                    .unwrap()
                    .data
                    .points
                    .len(),
                1
            );
        }
    }

    #[test]
    fn incompatible_preview_or_missing_hold_hides_markers_without_blocking_pan() {
        use crate::analysis_tools::{AnalysisConfig, MarkerTarget};
        for display in [S11Display::Resistance, S11Display::Smith] {
            for missing_hold in [false, true] {
                let mut app = two_marker_traces(display);
                for trace in &mut app.state.workspace.traces {
                    if missing_hold {
                        let markers = trace.analysis.markers().to_vec();
                        trace.analysis.restore_config(&AnalysisConfig {
                            markers,
                            target: MarkerTarget::Hold,
                            ..Default::default()
                        });
                    } else {
                        let old = trace.preview.take().unwrap();
                        trace.preview = Some(Arc::new(crate::preview::PreviewEnvelope {
                            session_id: old.session_id + 1,
                            request_id: old.request_id,
                            cycle_id: old.cycle_id,
                            data: old.data.clone(),
                            group: old.group.clone(),
                        }));
                    }
                }
                let ctx = egui::Context::default();
                let size = egui::vec2(800.0, 600.0);
                let output = interaction_frame(&ctx, size, vec![], 0.0, |ui| app.plot_ui(ui));
                assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().contains(" M1"))));
                output.drop_without_applying_deltas();
                let before_x = app.state.workspace.x_view.x_min;
                let before_smith = app.state.workspace.smith;
                let from = egui::pos2(300.0, 350.0);
                let to = from + egui::vec2(40.0, 30.0);
                for (index, events) in [
                    vec![egui::Event::PointerMoved(from)],
                    vec![pointer_button(from, true)],
                    vec![egui::Event::PointerMoved(to)],
                    vec![pointer_button(to, false)],
                ]
                .into_iter()
                .enumerate()
                {
                    interaction_frame(&ctx, size, events, (index + 1) as f64 * 0.02, |ui| {
                        app.plot_ui(ui)
                    })
                    .drop_without_applying_deltas();
                }
                if display == S11Display::Smith {
                    assert_ne!(app.state.workspace.smith, before_smith);
                } else {
                    assert_ne!(app.state.workspace.x_view.x_min, before_x);
                }
                assert_eq!(app.state.workspace.selected, Some(TraceId(1)));
            }
        }
    }

    #[test]
    fn mixed_chart_divider_drag_and_window_resize_obey_responsive_bounds() {
        let mut app = active_impedance_app();
        app.state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Smith),
                ..Default::default()
            })
            .unwrap();
        let ctx = egui::Context::default();
        let size = egui::vec2(800.0, 600.0);
        let mut chart_top = |events, time, size| {
            let output = interaction_frame(&ctx, size, events, time, |ui| app.plot_ui(ui));
            let top = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::LineSegment { points, .. }
                        if points[0].y == points[1].y
                            && (points[1].x - points[0].x).abs() > size.x * 0.95 =>
                    {
                        Some(points[0].y)
                    }
                    _ => None,
                })
                .expect("mixed chart divider");
            output.drop_without_applying_deltas();
            top
        };
        let before = chart_top(vec![], 0.0, size);
        assert_eq!(chart_top(vec![], 0.01, size), before);
        let from = egui::pos2(400.0, 300.0);
        let to = egui::pos2(400.0, 420.0);
        for (index, events) in [
            vec![egui::Event::PointerMoved(from)],
            vec![pointer_button(from, true)],
            vec![egui::Event::PointerMoved(to)],
            vec![pointer_button(to, false)],
        ]
        .into_iter()
        .enumerate()
        {
            chart_top(events, (index + 1) as f64 * 0.02, size);
        }
        let after = chart_top(vec![], 0.2, size);
        assert!(
            after > before + 40.0,
            "divider did not move: {before} {after}"
        );
        assert!(after < 470.0);
        let shrunk = chart_top(vec![], 0.3, egui::vec2(800.0, 300.0));
        assert!(
            (75.0..250.0).contains(&shrunk),
            "unbounded split after window resize: {shrunk}"
        );
    }

    #[test]
    fn shrinking_a_resized_mixed_workspace_keeps_the_smith_circle_usable() {
        for language in Language::ALL {
            for theme_mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                let ctx = egui::Context::default();
                theme::setup(&ctx);
                theme::apply(&ctx, theme_mode);
                i18n::set_language(&ctx, language);
                let mut app = active_impedance_app();
                app.state.language = language;
                app.state
                    .workspace
                    .add_trace(TraceSettings {
                        display: TraceDisplay::S11(S11Display::Smith),
                        ..Default::default()
                    })
                    .unwrap();
                let large = egui::vec2(1280.0, 850.0);
                let divider = |output: &egui::FullOutput| {
                    output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::LineSegment { points, .. }
                                if points[0].y == points[1].y
                                    && points[0].x > 200.0
                                    && points[1].x < large.x - 200.0
                                    && points[1].x - points[0].x > large.x - 530.0
                                    && points[0].y > 150.0 =>
                            {
                                Some(points[0].y)
                            }
                            _ => None,
                        })
                        .expect("mixed workspace divider")
                };
                let output =
                    interaction_frame(&ctx, large, vec![], 0.0, |ui| app.instrument_ui(ui));
                let before = divider(&output);
                output.drop_without_applying_deltas();
                let from = egui::pos2(large.x * 0.5, before);
                let to = from + egui::vec2(0.0, 85.0);
                for (index, events) in [
                    vec![egui::Event::PointerMoved(from)],
                    vec![pointer_button(from, true)],
                    vec![egui::Event::PointerMoved(to)],
                    vec![pointer_button(to, false)],
                ]
                .into_iter()
                .enumerate()
                {
                    interaction_frame(&ctx, large, events, (index + 1) as f64 * 0.02, |ui| {
                        app.instrument_ui(ui)
                    })
                    .drop_without_applying_deltas();
                }
                let output =
                    interaction_frame(&ctx, large, vec![], 0.12, |ui| app.instrument_ui(ui));
                assert!(divider(&output) > before + 50.0);
                output.drop_without_applying_deltas();
                for frame in 0..3 {
                    let output = interaction_frame(
                        &ctx,
                        egui::vec2(960.0, 600.0),
                        vec![],
                        0.2 + frame as f64 * 0.02,
                        |ui| app.instrument_ui(ui),
                    );
                    let (circle, clip) = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Circle(circle) => Some((circle, shape.clip_rect)),
                            _ => None,
                        })
                        .max_by(|(left, _), (right, _)| left.radius.total_cmp(&right.radius))
                        .expect("Smith unit circle");
                    assert!(
                        circle.radius >= 40.0,
                        "Smith radius shrank to {}",
                        circle.radius
                    );
                    assert!(clip.contains_rect(egui::Rect::from_center_size(
                        circle.center,
                        egui::Vec2::splat(circle.radius * 2.0)
                    )));
                    assert_eq!(app.state.workspace.smith.zoom, 1.0);
                    output.drop_without_applying_deltas();
                }
            }
        }
    }

    #[test]
    fn marker_center_replaces_only_active_acquisition_and_rejects_old_deliveries() {
        let mut app = active_impedance_app();
        let old_delivery = completion(&app, 0, 2);
        let old_cancel = app.state.acquisition_cancel.clone();
        let old_request = app.state.request_id;
        let old_data = complete_data(&app);
        app.state.workspace.center_on_marker(3e6).unwrap();
        app.state.reconcile_plan();
        assert!(old_cancel.is_cancelled());
        assert!(app.state.request_id > old_request);
        assert_eq!(app.state.workspace.range.center_hz, 3e6);
        assert_eq!(app.state.workspace.range.span_hz, 1e6);
        app.apply_worker_event(old_delivery);
        assert_eq!(complete_data(&app), old_data);

        app.state.send(WorkerCommand::StopSweep);
        app.state.sweep = SweepState::Idle;
        let request = app.state.request_id;
        app.state.workspace.center_on_marker(4e6).unwrap();
        app.state.reconcile_plan();
        assert_eq!(app.state.request_id, request);
        assert_eq!(app.state.sweep, SweepState::Idle);
        assert!(app.state.active_plan.is_none());
    }

    #[test]
    fn automatic_marker_tracks_only_accepted_complete_snapshots_without_restarting() {
        use crate::analysis_tools::AnalysisConfig;
        use crate::widgets::plot::Marker;
        let mut app = active_impedance_app();
        app.state
            .workspace
            .selected_mut()
            .unwrap()
            .analysis
            .restore_config(&AnalysisConfig {
                markers: vec![Marker {
                    id: 1,
                    frequency_hz: 1_500_000.0,
                    selected: true,
                    auto_peak: true,
                    ..Default::default()
                }],
                column: 1,
                ..Default::default()
            });
        let request = app.state.request_id;
        let mut partial = prefix(&app, 0, 2, 2);
        partial.data.points[1].values[1] = 1000.0;
        app.apply_preview(partial);
        assert_eq!(
            app.state.workspace.selected().unwrap().analysis.markers()[0].frequency_hz,
            1_500_000.0
        );
        let mut delivery = completion(&app, 0, 2);
        if let WorkerEvent::SweepTrace(result) = &mut delivery.event {
            Arc::make_mut(&mut result.snapshot).data.points[2].values[1] = 100.0;
        }
        app.apply_worker_event(delivery);
        assert_eq!(
            app.state.workspace.selected().unwrap().analysis.markers()[0].frequency_hz,
            2_000_000.0
        );
        app.apply_worker_event(completion(&app, 0, 1));
        assert_eq!(
            app.state.workspace.selected().unwrap().analysis.markers()[0].frequency_hz,
            2_000_000.0
        );
        app.state.reconcile_plan();
        assert_eq!(app.state.request_id, request);
    }

    #[test]
    fn restored_manual_delay_view_survives_first_completed_render() {
        for locked in [false, true] {
            let mut original = active_impedance_app();
            let trace = original.state.workspace.selected_mut().unwrap();
            let mut settings = trace.settings.clone();
            settings.display = TraceDisplay::S21(S21Display::Delay);
            trace.update_settings(settings);
            trace.view.y_min = -2e-18;
            trace.view.y_max = 6e-18;
            trace.view.y_divisions = 16;
            trace.view_locked = locked;
            original.state.workspace.x_view.x_min = 1_100_000.0;
            original.state.workspace.x_view.x_max = 1_900_000.0;
            let saved = AppConfig::from_state(&original.state);
            let mut state = AppState::default();
            saved.apply_to(&mut state);
            assert_eq!(state.sweep, SweepState::Idle);
            assert_eq!(state.connection, ConnectionState::Disconnected);
            state.connection = ConnectionState::Connected;
            state.session_id = 1;
            state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
            let mut app = test_app(state);
            app.apply_worker_event(completion(&app, 0, 1));
            plot_frame(&mut app);
            let view = &app.state.workspace.selected().unwrap().view;
            assert_eq!((view.x_min, view.x_max), (1_100_000.0, 1_900_000.0));
            assert_eq!(view.y_divisions, 16);
            if locked {
                assert_eq!((view.y_min, view.y_max), (-2e-18, 6e-18));
            } else {
                assert!(view.y_min < -5e-9);
                assert!(view.y_max > -5e-9);
            }
        }
    }

    #[test]
    fn transmission_series_keep_signed_raw_values_and_seconds_in_both_languages() {
        for (display, format, values, expected, unit) in [
            (S21Display::Phase, "ma", vec![0.5, -90.0], -90.0, "deg"),
            (S21Display::Loss, "loss", vec![-3.0], -3.0, "dB"),
            (S21Display::Delay, "delay", vec![-5e-9], -5e-9, "s"),
        ] {
            for language in Language::ALL {
                let mut data = SweepData {
                    mode: StreamMode::S21,
                    format: format.into(),
                    points: vec![SweepPoint {
                        freq_hz: 1e6,
                        values: values.clone(),
                    }],
                };
                let settings = TraceSettings {
                    display: TraceDisplay::S21(display),
                    ..Default::default()
                };
                let series = trace_series(&settings, Some(&data), language, true);
                assert_eq!(series[0].points, [(1e6, expected)]);
                assert_eq!(series[0].name, display.label(language));
                assert_eq!(settings.display.unit(), unit);
                data.mode = StreamMode::S11;
                assert!(trace_series(&settings, Some(&data), language, true).is_empty());
            }
        }
    }

    #[test]
    fn same_wire_s11_and_s21_results_stay_with_their_own_members_and_requests() {
        let mut app = active_impedance_app();
        app.state.workspace.selected_mut().unwrap().settings.display =
            TraceDisplay::S11(S11Display::Phase);
        let transmission = app
            .state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S21(S21Display::Phase),
                ..Default::default()
            })
            .unwrap();
        app.state.reconcile_plan();
        assert_eq!(app.state.active_plan.as_ref().unwrap().groups.len(), 2);
        app.apply_worker_event(completion(&app, 0, 1));
        assert!(app.state.workspace.selected().unwrap().completed.is_none());
        app.apply_preview(prefix(&app, 1, 2, 2));
        assert!(app.state.workspace.traces[0].preview.is_none());
        app.apply_worker_event(completion(&app, 1, 2));
        let complete = app
            .state
            .workspace
            .selected()
            .unwrap()
            .completed
            .clone()
            .unwrap();
        assert_eq!(complete.data.mode, StreamMode::S21);
        assert_eq!(
            app.state.workspace.traces[0]
                .completed
                .as_ref()
                .unwrap()
                .data
                .mode,
            StreamMode::S11
        );
        let stale = completion(&app, 1, 3);
        let request = app.state.request_id;
        app.state.workspace.selected = Some(transmission);
        app.state.workspace.selected_mut().unwrap().settings.lo = kcsdi_core::commands::Lo::LowLo;
        app.state.reconcile_plan();
        assert_eq!(app.state.request_id, request + 1);
        app.apply_worker_event(stale);
        assert!(Arc::ptr_eq(
            app.state
                .workspace
                .selected()
                .unwrap()
                .completed
                .as_ref()
                .unwrap(),
            &complete
        ));
        assert!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .last_completed_cycle
                .is_none()
        );
    }

    #[test]
    fn transmission_delay_fit_preserves_a_nanosecond_range() {
        let mut app = active_impedance_app();
        let trace = app.state.workspace.selected_mut().unwrap();
        let mut settings = trace.settings.clone();
        settings.display = TraceDisplay::S21(S21Display::Delay);
        trace.update_settings(settings);
        app.state.reconcile_plan();
        let mut event = completion(&app, 0, 1);
        if let WorkerEvent::SweepTrace(delivery) = &mut event.event {
            for (point, delay) in Arc::make_mut(&mut delivery.snapshot)
                .data
                .points
                .iter_mut()
                .zip([-5e-9, 0.0, 8e-9])
            {
                point.values = vec![delay];
            }
        }
        app.apply_worker_event(event);
        plot_frame(&mut app);
        let trace = app.state.workspace.selected().unwrap();
        assert!(trace.view.y_min < -5e-9 && trace.view.y_max > 8e-9);
        assert!(trace.view.y_max - trace.view.y_min < 20e-9);
        assert_eq!(
            trace.completed.as_ref().unwrap().data.points[0].values,
            [-5e-9]
        );
    }

    #[test]
    fn shared_group_routes_one_snapshot_without_advancing_other_group_watermarks() {
        let mut app = active_impedance_app();
        let first = app.state.workspace.selected.unwrap();
        let second = app
            .state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Smith),
                ..Default::default()
            })
            .unwrap();
        let spec = app
            .state
            .workspace
            .add_trace(TraceSettings::default())
            .unwrap();
        app.state.reconcile_plan();
        assert_eq!(app.state.active_plan.as_ref().unwrap().groups.len(), 2);
        app.apply_preview(prefix(&app, 0, 2, 2));
        app.apply_preview(prefix(&app, 1, 3, 1));
        let a = app.state.workspace.traces[0].preview.as_ref().unwrap();
        let b = app.state.workspace.traces[1].preview.as_ref().unwrap();
        assert!(Arc::ptr_eq(a, b));
        app.apply_worker_event(completion(&app, 0, 1));
        for id in [first, second] {
            let trace = app
                .state
                .workspace
                .traces
                .iter()
                .find(|trace| trace.id == id)
                .unwrap();
            assert_eq!(trace.last_completed_cycle, Some(1));
            assert_eq!(trace.preview.as_ref().unwrap().cycle_id, 2);
        }
        let a = app.state.workspace.traces[0].completed.as_ref().unwrap();
        let b = app.state.workspace.traces[1].completed.as_ref().unwrap();
        assert!(Arc::ptr_eq(a, b));
        let other = app
            .state
            .workspace
            .traces
            .iter()
            .find(|trace| trace.id == spec)
            .unwrap();
        assert_eq!(other.last_completed_cycle, None);
        assert_eq!(other.preview.as_ref().unwrap().cycle_id, 3);
        app.apply_worker_event(completion(&app, 0, 2));
        assert!(app.state.workspace.traces[0].preview.is_none());
        assert!(app.state.workspace.traces[1].preview.is_none());
        assert!(app.state.workspace.traces[2].preview.is_some());
        app.apply_preview(prefix(&app, 0, 2, 2));
        assert!(app.state.workspace.traces[0].preview.is_none());
    }

    #[test]
    fn invalid_or_unowned_deliveries_do_not_poison_completed_snapshots() {
        let mut app = active_impedance_app();
        let original = complete_data(&app);
        for kind in 0..5 {
            let mut event = completion(&app, 0, 2);
            if let WorkerEvent::SweepTrace(delivery) = &mut event.event {
                match kind {
                    0 => delivery.members.push(TraceId(999)),
                    1 => Arc::make_mut(&mut delivery.snapshot).data.format = "ma".into(),
                    2 => {
                        Arc::make_mut(&mut delivery.snapshot).data.points.pop();
                    }
                    3 => Arc::make_mut(&mut delivery.snapshot).session_id += 1,
                    _ => event.cycle_id = None,
                }
            }
            app.apply_worker_event(event);
            assert_eq!(complete_data(&app), original);
            assert!(
                app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .last_completed_cycle
                    .is_none()
            );
        }
    }

    #[test]
    fn stop_edit_restart_rejects_old_same_shape_results_and_errors() {
        let mut app = active_impedance_app();
        let stale = completion(&app, 0, 2);
        let stale_preview = prefix(&app, 0, 3, 2);
        let old_request = app.state.request_id;
        app.state.send(WorkerCommand::StopSweep);
        app.state.workspace.selected_mut().unwrap().settings.cal =
            kcsdi_core::commands::Cal::CalSys;
        app.state.send(WorkerCommand::RunWorkspace(
            app.state.workspace.plan().unwrap(),
        ));
        app.apply_worker_event(stale);
        app.apply_preview(stale_preview);
        app.apply_worker_event(EventEnvelope {
            session_id: 1,
            request_id: old_request,
            cycle_id: None,
            event: WorkerEvent::Error("old acquisition rejected".into()),
        });
        assert!(app.state.any_running());
        assert!(app.state.status_message.is_none());
        assert_eq!(complete_data(&app), Some(completed_impedance()));
        assert!(app.state.workspace.selected().unwrap().preview.is_none());
        assert!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .last_completed_cycle
                .is_none()
        );
        app.apply_worker_event(completion(&app, 0, 4));
        assert_eq!(
            app.state.workspace.selected().unwrap().last_completed_cycle,
            Some(4)
        );
    }

    #[test]
    fn edits_are_checked_even_before_plan_reconciliation_and_deleted_ids_never_receive_data() {
        let mut app = active_impedance_app();
        let event = completion(&app, 0, 1);
        app.state.workspace.selected_mut().unwrap().settings.rbw = kcsdi_core::model::Rbw::R3k;
        app.apply_worker_event(event);
        assert!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .last_completed_cycle
                .is_none()
        );
        let stale = completion(&app, 0, 2);
        let old_id = app.state.workspace.selected.unwrap();
        app.state.workspace.remove_trace(old_id);
        let id = app
            .state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Impedance),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(id, old_id);
        app.apply_worker_event(stale);
        assert!(app.state.workspace.selected().unwrap().completed.is_none());
    }

    #[test]
    fn stop_requires_the_current_workers_acknowledgement() {
        let mut app = active_impedance_app();
        let old_request = app.state.request_id;
        app.state.send(WorkerCommand::StopSweep);
        for request in [old_request, app.state.request_id] {
            app.apply_worker_event(EventEnvelope {
                session_id: 1,
                request_id: request,
                cycle_id: None,
                event: WorkerEvent::SweepStopped,
            });
            assert_eq!(
                app.state.sweep,
                if request == old_request {
                    SweepState::Stopping
                } else {
                    SweepState::Idle
                }
            );
        }
        assert_eq!(complete_data(&app), Some(completed_impedance()));
        assert_eq!(app.state.connection, ConnectionState::Connected);
    }

    #[test]
    fn old_holds_are_not_drawn_with_new_condition_previews() {
        for display in [
            TraceDisplay::S11(S11Display::Impedance),
            TraceDisplay::S11(S11Display::Smith),
            TraceDisplay::Spec,
        ] {
            let mut app = active_impedance_app();
            app.state.workspace.selected_mut().unwrap().settings.display = display;
            app.state.reconcile_plan();
            app.apply_worker_event(completion(&app, 0, 1));
            let ctx = egui::Context::default();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 700.0));
            let mut draw = |events| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let trace = app.state.workspace.selected_mut().unwrap();
                        trace.analysis.controls_for_display(
                            ui,
                            Language::English,
                            trace.completed.as_deref(),
                            display.is_smith(),
                            300e6,
                            true,
                        );
                    },
                )
            };
            let mut target = None;
            for _ in 0..2 {
                let output = draw(Vec::new());
                target = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == "HOLD" => {
                            Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                        }
                        _ => None,
                    })
                    .next_back();
                output.drop_without_applying_deltas();
            }
            let pos = target.unwrap();
            for pressed in [true, false] {
                draw(vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ])
                .drop_without_applying_deltas();
            }
            let snapshot = app
                .state
                .workspace
                .selected()
                .unwrap()
                .completed
                .clone()
                .unwrap();
            assert!(
                app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .analysis
                    .held_trace()
                    .is_some()
            );
            let settings = &mut app.state.workspace.selected_mut().unwrap().settings;
            if display == TraceDisplay::Spec {
                settings.ref_level_dbm -= 10;
            } else {
                settings.cal = kcsdi_core::commands::Cal::CalSys;
            }
            app.state.reconcile_plan();
            app.apply_preview(prefix(&app, 0, 2, 0));
            assert!(
                app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .overlays_compatible()
            );
            app.apply_preview(prefix(&app, 0, 2, 2));
            assert!(
                !app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .overlays_compatible()
            );
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| app.plot_ui(ui),
            );
            assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().contains("Hold "))));
            output.drop_without_applying_deltas();
            assert!(Arc::ptr_eq(
                app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .completed
                    .as_ref()
                    .unwrap(),
                &snapshot
            ));
            assert!(
                app.state
                    .workspace
                    .selected()
                    .unwrap()
                    .analysis
                    .held_trace()
                    .is_some()
            );
        }
    }

    #[test]
    fn new_prefixes_do_not_fit_y_or_reset_the_shared_x_zoom() {
        for points in [0, 2] {
            let mut app = active_impedance_app();
            app.state.workspace.x_view.x_min = 1_100_000.0;
            app.state.workspace.x_view.x_max = 1_900_000.0;
            app.state.workspace.selected_mut().unwrap().needs_fit = true;
            let mut pending = prefix(&app, 0, 2, points);
            for point in &mut pending.data.points {
                point.values = vec![10_000.0, 10_000.0, 0.0];
            }
            app.apply_preview(pending);
            plot_frame(&mut app);
            let trace = app.state.workspace.selected().unwrap();
            assert!(!trace.needs_fit);
            assert!(trace.view.y_max < 100.0);
            assert_eq!(
                (trace.view.x_min, trace.view.x_max),
                (1_100_000.0, 1_900_000.0)
            );
            assert_eq!(complete_data(&app), Some(completed_impedance()));
            assert!(trace.preview.is_some());
            if points == 0 {
                assert_eq!(trace.display_data(), Some(&completed_impedance()));
            }
        }
    }

    #[test]
    fn first_spec_log_preview_keeps_the_full_stop_without_modifying_sampling() {
        let mut app = active_impedance_app();
        app.state.workspace.range = SweepRange::new(0.0, 2_000_000.0, 3);
        app.state.workspace.x_view.x_min = 0.0;
        app.state.workspace.x_view.x_max = 2_000_000.0;
        app.state.workspace.log_x = true;
        let trace = app.state.workspace.selected_mut().unwrap();
        trace.settings.display = TraceDisplay::Spec;
        trace.completed = None;
        app.state.reconcile_plan();
        let settings = app.state.active_plan.as_ref().unwrap().clone();
        let mut pending = prefix(&app, 0, 1, 2);
        for (index, point) in pending.data.points.iter_mut().enumerate() {
            point.freq_hz = index as f64 * 1_000_000.0;
        }
        app.apply_preview(pending);
        plot_frame(&mut app);
        assert_eq!(
            (
                app.state.workspace.x_view.x_min,
                app.state.workspace.x_view.x_max
            ),
            (1_000_000.0, 2_000_000.0)
        );
        assert_eq!(app.state.active_plan.as_ref(), Some(&settings));
        assert_eq!(app.state.workspace.range.start_hz, 0.0);
        assert_eq!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .preview
                .as_ref()
                .unwrap()
                .data
                .points[0]
                .freq_hz,
            0.0
        );
    }

    #[test]
    fn double_click_fit_uses_completed_data_or_full_configured_range_not_a_prefix() {
        for completed in [false, true] {
            let mut app = active_impedance_app();
            if !completed {
                app.state.workspace.selected_mut().unwrap().completed = None;
            }
            app.state.workspace.selected_mut().unwrap().view_locked = true;
            app.state.workspace.x_view.x_min = 1_100_000.0;
            app.state.workspace.x_view.x_max = 1_900_000.0;
            let mut pending = prefix(&app, 0, 2, 2);
            for point in &mut pending.data.points {
                point.values = vec![10_000.0, 10_000.0, 0.0];
            }
            app.apply_preview(pending);
            let ctx = egui::Context::default();
            let cursor = egui::pos2(350.0, 300.0);
            let mut frame = |time, events| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(800.0, 600.0),
                        )),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| app.plot_ui(ui),
                )
                .drop_without_applying_deltas();
            };
            frame(0.0, Vec::new());
            frame(0.02, vec![egui::Event::PointerMoved(cursor)]);
            for (index, pressed) in [true, false, true, false].into_iter().enumerate() {
                frame(
                    0.04 + index as f64 * 0.02,
                    vec![egui::Event::PointerButton {
                        pos: cursor,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
            }
            let trace = app.state.workspace.selected().unwrap();
            assert!(!trace.view_locked);
            assert_eq!(
                (
                    app.state.workspace.x_view.x_min,
                    app.state.workspace.x_view.x_max
                ),
                (1_000_000.0, 2_000_000.0)
            );
            assert_eq!(
                (trace.view.x_min, trace.view.x_max),
                (1_000_000.0, 2_000_000.0)
            );
            assert!(trace.view.y_max <= 200.0);
            if !completed {
                assert_eq!(
                    (trace.view.y_min, trace.view.y_max),
                    S11Display::Impedance.default_y()
                );
            }
        }
    }

    #[test]
    fn spectrum_fits_completed_data_while_next_group_is_partial() {
        let mut app = active_impedance_app();
        let id = app
            .state
            .workspace
            .add_trace(TraceSettings::default())
            .unwrap();
        app.state.reconcile_plan();
        app.apply_worker_event(completion(&app, 1, 2));
        let mut preview = prefix(&app, 1, 3, 2);
        for point in &mut preview.data.points {
            point.values = vec![10_000.0];
        }
        app.apply_preview(preview);
        plot_frame(&mut app);
        let trace = app
            .state
            .workspace
            .traces
            .iter()
            .find(|trace| trace.id == id)
            .unwrap();
        assert!(!trace.needs_fit);
        assert!(trace.view.y_max < 0.0);
        assert!(
            trace
                .completed
                .as_ref()
                .unwrap()
                .data
                .points
                .iter()
                .all(|point| point.values == [-20.0])
        );
    }

    #[test]
    fn wire_format_switch_preserves_export_but_does_not_render_old_columns() {
        let mut app = active_impedance_app();
        let trace = app.state.workspace.selected_mut().unwrap();
        let mut settings = trace.settings.clone();
        settings.display = TraceDisplay::S11(S11Display::Phase);
        trace.update_settings(settings);
        app.state.reconcile_plan();
        assert!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .completed_for_display()
                .is_none()
        );
        let mut preview = prefix(&app, 0, 2, 2);
        for point in &mut preview.data.points {
            point.values = vec![0.5, 10.0];
        }
        app.apply_preview(preview);
        plot_frame(&mut app);
        assert_eq!(complete_data(&app), Some(completed_impedance()));
        assert_eq!(
            app.state
                .workspace
                .selected()
                .unwrap()
                .display_data()
                .unwrap()
                .format,
            "ma"
        );
    }

    #[test]
    fn disconnect_clears_health_and_previews_but_keeps_completed_snapshots() {
        for lost in [false, true] {
            let mut app = active_impedance_app();
            app.apply_preview(prefix(&app, 0, 2, 2));
            app.apply_event(if lost {
                WorkerEvent::ConnectionLost("connection reset".into())
            } else {
                WorkerEvent::Disconnected
            });
            assert!(app.state.health.snapshot.is_none());
            assert!(!app.state.health.pending);
            assert!(app.state.workspace.selected().unwrap().preview.is_none());
            assert_eq!(complete_data(&app), Some(completed_impedance()));
            assert_eq!(app.state.sweep, SweepState::Idle);
        }
    }
    #[test]
    fn close_is_cancelled_until_the_owned_worker_finishes() {
        let mut app = active_impedance_app();
        let (commands, receiver) = mpsc::channel();
        app.state.cmd_tx = Some(commands);
        let (release, wait) = mpsc::channel();
        app.worker = Some(std::thread::spawn(move || {
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
        }));
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .events
                .push(egui::ViewportEvent::Close);
            let mut output = ctx.run_ui(input, |ui| app.poll_close(ui.ctx()));
            output.textures_delta.clear();
            let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(commands.contains(&egui::ViewportCommand::CancelClose));
            assert!(!commands.contains(&egui::ViewportCommand::Close));
            assert!(app.state.worker_shutdown.is_cancelled());
        }
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            crate::state::WorkerCommand::Shutdown
        ));
        assert!(receiver.try_recv().is_err());
        release.send(()).unwrap();
        let started = Instant::now();
        while !app.worker.as_ref().unwrap().is_finished() {
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::yield_now();
        }
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.poll_close(ui.ctx()));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        assert!(app.worker.is_none());
        assert!(app.state.cmd_tx.is_none());
    }

    #[test]
    fn metadata_cleanup_never_delays_dispatching_device_shutdown() {
        let (tx, rx) = mpsc::channel();
        let mut app = test_app(AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        });
        let (release, wait) = mpsc::channel();
        app.state.desktop.lookup.ports.start_test(move |cancel| {
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(cancel.is_cancelled());
            Ok(Vec::new())
        });
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run_ui(input, |ui| app.poll_close(ui.ctx()));
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::Shutdown
        ));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        output.drop_without_applying_deltas();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.state.desktop.lookup.is_pending() {
            assert!(Instant::now() < deadline);
            app.state.desktop.lookup.poll();
            std::thread::yield_now();
        }
        let output = ctx.run_ui(egui::RawInput::default(), |ui| app.poll_close(ui.ctx()));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        output.drop_without_applying_deltas();
    }

    #[test]
    fn pending_folder_launch_never_delays_device_shutdown_or_host_close() {
        let (tx, rx) = mpsc::channel();
        let mut app = test_app(AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        });
        let (release, ready) = app.state.folder_opener.start_test_blocked();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        let started = Instant::now();
        let mut output = ctx.run_ui(input, |ui| app.poll_close(ui.ctx()));
        output.textures_delta.clear();
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::Shutdown
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        output.drop_without_applying_deltas();
        // Host cancellation completes even while the native manager stays blocked.
        let deadline = Instant::now() + Duration::from_secs(1);
        while app.state.folder_opener.is_pending() {
            assert!(Instant::now() < deadline);
            assert!(app.state.folder_opener.poll().is_none());
            std::thread::yield_now();
        }
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.poll_close(ui.ctx()));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        output.drop_without_applying_deltas();
        release.send(()).unwrap();
    }

    #[test]
    fn dropping_the_ui_cancels_all_worker_operations() {
        let app = active_impedance_app();
        let acquisition = app.state.acquisition_cancel.clone();
        let session = app.state.session_cancel.clone();
        let shutdown = app.state.worker_shutdown.clone();
        drop(app);
        assert!(acquisition.is_cancelled());
        assert!(session.is_cancelled());
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn connection_lifecycle_rejects_old_session_events_before_worker_acknowledgement() {
        for reconnect in [false, true] {
            let mut app = active_impedance_app();
            let old_session = app.state.session_id;
            app.state.send(crate::state::WorkerCommand::Disconnect);
            if reconnect {
                app.state.send(crate::state::WorkerCommand::Connect {
                    target: kcsdi_core::connection::ConnectionTarget::Tcp {
                        host: "instrument.local".into(),
                        port: 901,
                    },
                });
            }
            for event in [
                WorkerEvent::Connected(kcsdi_core::data::DeviceInfo {
                    serial: "old-session".into(),
                    username: String::new(),
                    software: String::new(),
                    hardware: String::new(),
                    copyright: String::new(),
                }),
                WorkerEvent::Disconnected,
                WorkerEvent::Status(crate::health::tests::snapshot(99.0, Instant::now())),
                WorkerEvent::StatusFailed("old status error".into()),
                WorkerEvent::ConnectionLost("old socket closed".into()),
            ] {
                let request_id = app.state.request_id;
                app.apply_worker_event(EventEnvelope {
                    session_id: old_session,
                    request_id,
                    cycle_id: None,
                    event,
                });
                assert_eq!(
                    app.state.connection,
                    if reconnect {
                        ConnectionState::Connecting
                    } else {
                        ConnectionState::Disconnecting
                    }
                );
                assert!(app.state.device_info.is_none());
                assert!(app.state.health.snapshot.is_none());
                assert!(app.state.health.error.is_none());
                assert!(!app.state.health.pending);
                assert_eq!(app.state.request_id, request_id);
                assert!(!app.state.any_running());
                assert_eq!(complete_data(&app), Some(completed_impedance()));
            }
        }
    }

    #[test]
    fn connection_loss_from_an_old_request_still_invalidates_the_current_session() {
        let mut app = active_impedance_app();
        app.state.request_id = 3;
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: 1,
            cycle_id: None,
            event: WorkerEvent::ConnectionLost("shared socket closed".into()),
        });
        assert!(matches!(app.state.connection, ConnectionState::Error(_)));
        assert!(app.state.health.snapshot.is_none());
        assert!(!app.state.any_running());
        assert_eq!(complete_data(&app), Some(completed_impedance()));
    }

    #[test]
    fn health_events_follow_the_session_instead_of_the_measurement_request() {
        let mut app = active_impedance_app();
        app.state.request_id = 3;
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: 1,
            cycle_id: None,
            event: WorkerEvent::Status(crate::health::tests::snapshot(43.0, Instant::now())),
        });
        assert_eq!(
            app.state.health.snapshot.as_ref().unwrap().temperature,
            43.0
        );
        assert!(app.state.any_running());
        assert_eq!(app.state.request_id, 3);
    }

    #[test]
    fn status_failure_preserves_running_job_and_last_complete_readings() {
        let mut app = active_impedance_app();
        app.state.request_id = 3;
        app.state.status_message = Some("An unrelated message".to_string().into());
        app.state.health.pending = true;
        let original = app
            .state
            .workspace
            .selected()
            .unwrap()
            .completed
            .clone()
            .unwrap();
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: 1,
            cycle_id: None,
            event: WorkerEvent::StatusFailed("Status query failed: err_cmd".into()),
        });
        assert!(app.state.any_running());
        assert!(app.state.active_plan.is_some());
        assert_eq!(
            app.state.health.snapshot.as_ref().unwrap().temperature,
            42.0
        );
        assert!(app.state.health.is_stale(Instant::now()));
        assert!(!app.state.health.pending);
        assert!(app.state.health.error.is_some());
        assert!(Arc::ptr_eq(
            &original,
            app.state
                .workspace
                .selected()
                .unwrap()
                .completed
                .as_ref()
                .unwrap()
        ));
        app.apply_worker_event(EventEnvelope {
            session_id: app.state.session_id,
            request_id: 1,
            cycle_id: None,
            event: WorkerEvent::Status(crate::health::tests::snapshot(44.0, Instant::now())),
        });
        assert!(app.state.health.error.is_none());
        assert!(!app.state.health.is_stale(Instant::now()));
        assert_eq!(
            app.state
                .status_message
                .as_ref()
                .unwrap()
                .text(Language::English),
            "An unrelated message"
        );
    }

    #[test]
    fn delayed_health_event_keeps_query_age_but_starts_a_new_cooldown() {
        let mut app = active_impedance_app();
        let now = Instant::now();
        let observed_at = now - std::time::Duration::from_secs(60);
        app.apply_event(WorkerEvent::Status(crate::health::tests::snapshot(
            42.0,
            observed_at,
        )));
        assert_eq!(
            app.state.health.age(now),
            Some(std::time::Duration::from_secs(60))
        );
        assert!(app.state.health.is_stale(now));
        assert!(!app.state.health.due(now));
    }

    #[test]
    fn health_events_cannot_repopulate_an_inactive_current_session() {
        for connection in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Disconnecting,
            ConnectionState::Error("connection failed".into()),
        ] {
            let mut app = active_impedance_app();
            app.state.connection = connection;
            app.state.health = Default::default();
            for event in [
                WorkerEvent::Status(crate::health::tests::snapshot(99.0, Instant::now())),
                WorkerEvent::StatusFailed("late status failure".into()),
            ] {
                app.apply_worker_event(EventEnvelope {
                    session_id: app.state.session_id,
                    request_id: app.state.request_id,
                    cycle_id: None,
                    event,
                });
                assert!(app.state.health.snapshot.is_none());
                assert!(app.state.health.error.is_none());
                assert!(!app.state.health.pending);
            }
        }
    }

    #[test]
    fn about_keeps_connection_failures_visible_and_allows_reconnect() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            theme::setup(&ctx);
            let mut app = active_impedance_app();
            app.state.desktop.page = Page::About;
            app.state.language = language;
            app.state.target = kcsdi_core::connection::ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 901,
            };
            let (sender, receiver) = mpsc::channel();
            app.state.cmd_tx = Some(sender);
            app.apply_event(WorkerEvent::ConnectionLost("health query timeout".into()));
            let input = || egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                ..Default::default()
            };
            let text_position = |output: &egui::FullOutput, label: &str| {
                output.shapes.iter().find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
            };
            let mut output = ctx.run_ui(input(), |ui| app.desktop_ui(ui));
            output.textures_delta.clear();
            for _ in 0..2 {
                output = ctx.run_ui(input(), |ui| app.desktop_ui(ui));
                output.textures_delta.clear();
            }
            let connect = text_position(&output, language.text(Text::Connect)).unwrap();
            assert!(text_position(&output, language.text(Text::Error)).is_some());
            assert!(text_position(&output, "42.0 C").is_none());
            assert_eq!(
                app.state.status_message.as_ref().unwrap().text(language),
                "health query timeout"
            );
            let mut click = input();
            click.events.push(egui::Event::PointerMoved(connect));
            for pressed in [true, false] {
                click.events.push(egui::Event::PointerButton {
                    pos: connect,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                });
            }
            output = ctx.run_ui(click, |ui| app.desktop_ui(ui));
            output.textures_delta.clear();
            assert!(matches!(
                receiver.try_recv().unwrap().command,
                WorkerCommand::Connect { .. }
            ));
            assert!(receiver.try_recv().is_err());
            assert_eq!(app.state.connection, ConnectionState::Connecting);
            output = ctx.run_ui(input(), |ui| app.desktop_ui(ui));
            output.textures_delta.clear();
            assert!(text_position(&output, language.text(Text::Connecting)).is_some());
            app.apply_event(WorkerEvent::Disconnected);
            output = ctx.run_ui(input(), |ui| app.desktop_ui(ui));
            output.textures_delta.clear();
            assert!(text_position(&output, language.text(Text::Connect)).is_none());
            assert!(text_position(&output, language.text(Text::Error)).is_none());
        }
    }

    #[test]
    fn connection_window_is_explicit_and_fits_the_desktop_in_both_languages() {
        use kcsdi_core::connection::ConnectionTarget;
        for language in Language::ALL {
            for theme_mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
                    for target in [
                        ConnectionTarget::Tcp {
                            host: "instrument.local".into(),
                            port: 4321,
                        },
                        ConnectionTarget::Serial {
                            path: "/dev/serial/by-id/unavailable".into(),
                        },
                    ] {
                        let ctx = egui::Context::default();
                        theme::setup(&ctx);
                        theme::apply(&ctx, theme_mode);
                        let (tx, rx) = mpsc::channel();
                        let mut app = test_app(AppState {
                            language,
                            target: target.clone(),
                            cmd_tx: Some(tx),
                            ..Default::default()
                        });
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        let frame = |app: &mut KcsdiApp, events| {
                            ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    events,
                                    ..Default::default()
                                },
                                |ui| {
                                    app.desktop_ui(ui);
                                    egui::Window::new(language.text(Text::Connection))
                                        .resizable(false)
                                        .show(ui.ctx(), |ui| {
                                            panels::top_bar::show(ui, &mut app.state)
                                        });
                                },
                            )
                        };
                        let mut connect = None;
                        for _ in 0..3 {
                            let output = frame(&mut app, Vec::new());
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape {
                                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                                    let visible = rect.intersect(shape.clip_rect);
                                    if visible.is_positive() {
                                        assert!(
                                            screen.contains_rect(visible),
                                            "{language:?} {theme_mode:?}: {}",
                                            text.galley.text()
                                        );
                                    }
                                    if text.galley.text() == language.text(Text::Connect) {
                                        connect = Some(rect.center());
                                        assert!(shape.clip_rect.contains_rect(rect));
                                    }
                                }
                            }
                            output.drop_without_applying_deltas();
                        }
                        assert!(rx.try_recv().is_err());
                        assert!(!app.state.desktop.lookup.is_pending());
                        let at = connect.unwrap();
                        for pressed in [true, false] {
                            frame(
                                &mut app,
                                vec![
                                    egui::Event::PointerMoved(at),
                                    egui::Event::PointerButton {
                                        pos: at,
                                        button: egui::PointerButton::Primary,
                                        pressed,
                                        modifiers: Default::default(),
                                    },
                                ],
                            )
                            .drop_without_applying_deltas();
                        }
                        match rx.try_recv().unwrap().command {
                            WorkerCommand::Connect { target: requested } => {
                                assert_eq!(requested, target)
                            }
                            _ => panic!("expected explicit Connect"),
                        }
                        assert_eq!(app.state.connection, ConnectionState::Connecting);
                        assert!(rx.try_recv().is_err());
                    }
                }
            }
        }
    }

    #[test]
    fn populated_transmission_panels_stay_within_their_fixed_width() {
        for width in [1280.0, 960.0] {
            for language in Language::ALL {
                for theme_mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                    let ctx = egui::Context::default();
                    theme::setup(&ctx);
                    theme::apply(&ctx, theme_mode);
                    let mut app = active_impedance_app();
                    app.state.language = language;
                    app.state.workspace = Workspace::empty(SweepRange::new(1e6, 100e6, 201));
                    for display in [
                        S21Display::Delay,
                        S21Display::Phase,
                        S21Display::Loss,
                        S21Display::Loss,
                        S21Display::Loss,
                    ] {
                        app.state
                            .workspace
                            .add_trace(TraceSettings {
                                display: TraceDisplay::S21(display),
                                ..Default::default()
                            })
                            .unwrap();
                        let range = app.state.workspace.range;
                        let trace = app.state.workspace.selected_mut().unwrap();
                        let snapshot = Arc::new(CompletedSweep {
                            settings: trace.settings.acquisition(&range).unwrap(),
                            session_id: 1,
                            completed_at: SystemTime::UNIX_EPOCH,
                            data: SweepData {
                                mode: StreamMode::S21,
                                format: display.wire_format().as_str().into(),
                                points: (0..201)
                                    .map(|index| SweepPoint {
                                        freq_hz: 1e6 + f64::from(index) * 495000.0,
                                        values: match display {
                                            S21Display::Delay => vec![
                                                -5.147039e-9
                                                    + 8.39821e-9 * (f64::from(index) / 32.0).sin(),
                                            ],
                                            S21Display::Phase => vec![0.5, -90.0],
                                            S21Display::Loss => {
                                                vec![-123_456_789.123 + f64::from(index)]
                                            }
                                        },
                                    })
                                    .collect(),
                            },
                        });
                        trace.analysis.observe(&snapshot);
                        trace.completed = Some(snapshot);
                    }
                    let screen =
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 850.0));
                    for selected in [0, 2] {
                        let trace = &app.state.workspace.traces[selected];
                        let title =
                            format!("T{} {}", trace.id.0, trace.settings.display.label(language));
                        app.state.workspace.selected = Some(trace.id);
                        for _ in 0..3 {
                            let mut output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| app.instrument_ui(ui),
                            );
                            let shapes = std::mem::take(&mut output.shapes);
                            output.drop_without_applying_deltas();
                            let panel = egui::containers::panel::PanelState::load(
                                &ctx,
                                egui::Id::new("params_panel"),
                            )
                            .unwrap();
                            assert!(
                                (panel.outer_rect.left() - (width - 256.0)).abs() <= 1.0,
                                "{language:?} {theme_mode:?} panel {:?}",
                                panel.outer_rect
                            );
                            assert!(panel.outer_rect.right() <= width + 1.0);
                            for label in [&title, language.text(Text::CalSys)] {
                                let shape = shapes.iter().find(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == label)).expect("parameter label must remain visible");
                                let egui::Shape::Text(text) = &shape.shape else {
                                    unreachable!()
                                };
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    shape.clip_rect.contains_rect(bounds),
                                    "{label}: {bounds:?} outside {:?}",
                                    shape.clip_rect
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn instrument_layout_keeps_controls_in_their_panes_at_both_window_sizes() {
        for width in [960.0, 1280.0] {
            for language in Language::ALL {
                for theme_mode in [theme::ThemeMode::Light, theme::ThemeMode::Dark] {
                    let ctx = egui::Context::default();
                    theme::setup(&ctx);
                    theme::apply(&ctx, theme_mode);
                    i18n::set_language(&ctx, language);
                    let state = crate::state::AppState {
                        language,
                        ..Default::default()
                    };
                    let mut app = test_app(state);
                    let screen =
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0));
                    for _ in 0..3 {
                        let output = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |ui| app.instrument_ui(ui),
                        );
                        let labels: Vec<_> = output
                            .shapes
                            .iter()
                            .filter_map(|shape| match &shape.shape {
                                egui::Shape::Text(text) => Some((
                                    text.galley.text().to_owned(),
                                    text.galley.rect.translate(text.pos.to_vec2()),
                                )),
                                _ => None,
                            })
                            .collect();
                        output.drop_without_applying_deltas();
                        for (key, left) in [
                            (Text::TraceList, true),
                            (Text::Run, false),
                            (Text::Export, false),
                        ] {
                            let (_, bounds) = labels
                                .iter()
                                .find(|(text, _)| text == language.text(key))
                                .expect("control must be rendered");
                            assert!(
                                screen.contains_rect(*bounds),
                                "{language:?} {theme_mode:?} {key:?}: {bounds:?}"
                            );
                            if left {
                                assert!(bounds.right() < 256.0);
                            } else {
                                assert!(bounds.left() >= width - 256.0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn format_changes_do_not_reinterpret_old_impedance_columns() {
        let mut data = SweepData {
            mode: StreamMode::S11,
            format: "z".to_string(),
            points: vec![SweepPoint {
                freq_hz: 1e6,
                values: vec![50.0, 30.0, -40.0],
            }],
        };
        let series = cartesian_series(
            S11Display::Impedance,
            Some(&data),
            [true; 3],
            Language::English,
        );
        assert_eq!(series.len(), 3);
        assert_eq!(series[2].points, vec![(1e6, -40.0)]);
        for display in [S11Display::Phase, S11Display::ReturnLoss, S11Display::Vswr] {
            assert!(
                cartesian_series(display, Some(&data), [true; 3], Language::English).is_empty()
            );
        }
        data.mode = StreamMode::S21;
        assert!(
            cartesian_series(
                S11Display::Impedance,
                Some(&data),
                [true; 3],
                Language::English
            )
            .is_empty()
        );
    }

    #[test]
    fn impedance_visibility_defaults_to_all_and_survives_new_traces() {
        let mut state = TraceSettings::default();
        assert_eq!(state.impedance_visible, [true; 3]);
        let mut data = SweepData {
            mode: StreamMode::S11,
            format: "z".to_string(),
            points: vec![SweepPoint {
                freq_hz: 100_000.0,
                values: vec![50.0, 30.0, -40.0],
            }],
        };
        state.impedance_visible = [false, true, false];
        for magnitude in [50.0, 100.0] {
            data.points[0].values[0] = magnitude;
            let series = cartesian_series(
                S11Display::Impedance,
                Some(&data),
                state.impedance_visible,
                Language::English,
            );
            assert_eq!(
                series.iter().map(|s| s.visible).collect::<Vec<_>>(),
                [false, true, false]
            );
            assert_eq!(series[0].points, [(100_000.0, magnitude)]);
            assert_eq!(series[2].points, [(100_000.0, -40.0)]);
        }
        data.format = "ma".to_string();
        let series = cartesian_series(
            S11Display::Phase,
            Some(&data),
            [false; 3],
            Language::English,
        );
        assert!(series[0].visible);
    }

    #[test]
    fn phase_and_return_loss_keep_signed_measurement_values() {
        for (display, format, values, expected) in [
            (S11Display::Phase, "ma", vec![0.5, -90.0], -90.0),
            (S11Display::ReturnLoss, "loss", vec![-3.0], -3.0),
        ] {
            let data = SweepData {
                mode: StreamMode::S11,
                format: format.to_string(),
                points: vec![SweepPoint {
                    freq_hz: 1e6,
                    values,
                }],
            };
            let series = cartesian_series(display, Some(&data), [true; 3], Language::English);
            assert_eq!(series[0].points, vec![(1e6, expected)]);
        }
    }

    #[test]
    fn translating_series_changes_labels_not_measurements() {
        let data = SweepData {
            mode: StreamMode::S11,
            format: "ma".to_string(),
            points: vec![SweepPoint {
                freq_hz: 1e6,
                values: vec![0.5, -90.0],
            }],
        };
        for selected in Language::ALL {
            let series = cartesian_series(S11Display::Phase, Some(&data), [true; 3], selected);
            assert_eq!(series[0].name, selected.text(Text::Phase));
            assert_eq!(series[0].points, [(1e6, -90.0)]);
            assert_eq!(S11Display::Phase.y_label(), "deg");
            assert_eq!(S11Display::Phase.wire_format().as_str(), "ma");
        }
    }
}
