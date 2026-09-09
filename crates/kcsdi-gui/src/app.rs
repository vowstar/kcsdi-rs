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
            WorkerEvent::SweepTrace(_) | WorkerEvent::SweepStopped | WorkerEvent::Error(_)
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
        for trace in &mut self.state.workspace.traces {
            if !trace.settings.visible
                || !group.members.contains(&trace.id)
                || trace.settings.acquisition(&range).ok().as_ref() != Some(&group.settings)
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
        let preview = Arc::new(preview);
        for trace in &mut self.state.workspace.traces {
            if !trace.settings.visible
                || !preview.group.members.contains(&trace.id)
                || trace.settings.acquisition(&range).ok().as_ref() != Some(&preview.group.settings)
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
                info!("connected, serial {}", info.serial);
                self.state.connection = ConnectionState::Connected;
                self.state.device_info = Some(info);
                self.state.status_message = Some(StatusMessage::Text(Text::Connected));
                self.state.send(crate::state::WorkerCommand::RefreshStatus);
            }
            WorkerEvent::Disconnected => {
                self.clear_connection();
                self.state.status_message = Some(StatusMessage::Text(Text::Disconnected));
            }
            WorkerEvent::ConnectionLost(message) => {
                self.clear_connection();
                self.state.connection = ConnectionState::Error(message.clone());
                self.state.status_message = Some(message.into());
            }
            WorkerEvent::Error(message) => {
                if self.state.connection == ConnectionState::Connecting {
                    self.state.connection = ConnectionState::Error(message.clone());
                }
                self.state.sweep = SweepState::Idle;
                self.state.active_plan = None;
                self.state.clear_preview();
                self.state.status_message = Some(message.into());
            }
            WorkerEvent::SweepStopped => {
                if self.state.sweep == SweepState::Stopping {
                    self.state.sweep = SweepState::Idle;
                    self.state.clear_preview();
                }
            }
            WorkerEvent::SweepTrace(_) => {}
            WorkerEvent::Status {
                temperature,
                voltage,
            } => {
                self.state.temperature = Some(temperature);
                self.state.voltage = Some(voltage);
            }
        }
    }

    fn clear_connection(&mut self) {
        self.state.connection = ConnectionState::Disconnected;
        self.state.device_info = None;
        self.state.temperature = None;
        self.state.voltage = None;
        self.state.sweep = SweepState::Idle;
        self.state.active_plan = None;
        self.state.clear_preview();
        self.state.acquisition_cancel.cancel();
        self.state.session_cancel.cancel();
    }

    fn poll_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) && !self.closing {
            self.closing = true;
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
        if self.state.export.is_pending() {
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
        if self.closing {
            ui.disable();
            egui::Window::new(self.state.language.text(Text::Closing))
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.spinner();
                    ui.label(self.state.language.text(if self.state.export.is_pending() {
                        Text::ExportBusy
                    } else {
                        Text::Disconnecting
                    }));
                });
        }

        if ctx.input(|input| input.key_pressed(egui::Key::F11)) {
            let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if self.state.desktop.page == Page::Instrument {
            self.instrument_ui(ui);
        } else {
            if matches!(
                self.state.connection,
                ConnectionState::Connected | ConnectionState::Disconnecting
            ) {
                egui::Panel::bottom("desktop_session_status").show(ui, |ui| {
                    if panels::status_bar::show(ui, &mut self.state) {
                        self.connection_open = true;
                    }
                });
            }
            desktop::show_home(ui, &mut self.state);
        }

        egui::Window::new(self.state.language.text(Text::Connection))
            .open(&mut self.connection_open)
            .resizable(false)
            .collapsible(false)
            .show(&ctx, |ui| panels::top_bar::show(ui, &mut self.state));

        trace_editor(&ctx, &mut self.state);
        self.state.reconcile_plan();
        ctx.request_repaint_after(Duration::from_millis(500));
        self.persist_config_debounced();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_config();
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
        self.state.worker_shutdown.cancel();
        self.state.session_cancel.cancel();
        self.state.acquisition_cancel.cancel();
        self.state.cmd_tx = None;
    }
}

impl KcsdiApp {
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
                    if ui
                        .add_enabled(
                            self.state.workspace.traces.len() < crate::acquisition::MAX_TRACES,
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
            egui::Panel::top("cartesian_region")
                .exact_size(height * 0.5)
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
            let partial = trace
                .preview
                .as_ref()
                .is_some_and(|preview| !preview.data.points.is_empty());
            prepared.push((
                base_count,
                partial,
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
                |(trace, (_, partial, options, _))| widgets::plot::CartesianLayer {
                    id: trace.id.0,
                    label: format!("T{} {}", trace.id.0, trace.settings.display.label(language)),
                    view: &mut trace.view,
                    options: options.clone(),
                    line_width: trace.settings.line_width,
                    markers: if *partial {
                        &mut []
                    } else {
                        trace.analysis.markers_mut()
                    },
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
        let mut reset_x = None;
        for ((trace, (base_count, _, _, completed_options)), options) in workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && !trace.settings.display.is_smith())
            .zip(prepared)
            .zip(options)
        {
            if let Some((_, lock)) = changes.iter().find(|(id, _)| *id == trace.id.0) {
                match lock {
                    widgets::plot::ViewLock::Locked => trace.view_locked = true,
                    widgets::plot::ViewLock::Unlocked => {
                        trace.view_locked = false;
                        let (low, high) = trace.settings.display.default_y();
                        trace.view.reset(
                            workspace.range.start_hz,
                            workspace.range.stop_hz,
                            low,
                            high,
                        );
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
                )
            })
            .collect();
        let mut layers: Vec<_> = workspace
            .traces
            .iter_mut()
            .filter(|trace| trace.settings.visible && trace.settings.display.is_smith())
            .zip(prepared.iter())
            .map(|(trace, (data, held))| widgets::smith::SmithLayer {
                id: trace.id.0,
                label: format!("T{}", trace.id.0),
                trace: data.as_ref(),
                held: held.as_ref(),
                color: displayed_color(trace.settings.color, ui.visuals().dark_mode),
                line_width: trace.settings.line_width,
                markers: if trace
                    .preview
                    .as_ref()
                    .is_some_and(|preview| !preview.data.points.is_empty())
                {
                    &mut []
                } else {
                    trace.analysis.markers_mut()
                },
            })
            .collect();
        layers
            .sort_by_key(|layer| Some(crate::acquisition::TraceId(layer.id)) == workspace.selected);
        widgets::smith::show_multi(ui, &mut workspace.smith, &mut layers);
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
            let completed = state
                .workspace
                .selected()
                .and_then(|trace| trace.completed.clone());
            egui::Panel::bottom("trace_export")
                .default_size(128.0)
                .min_size(112.0)
                .show_separator_line(true)
                .show(ui, |ui| {
                    crate::export::show(ui, &mut state.export, completed.as_deref(), language)
                });
            egui::ScrollArea::vertical()
                .id_salt("analysis_scroll")
                .show(ui, |ui| {
                    let editable = state.sweep != SweepState::Stopping
                        && state.connection != ConnectionState::Disconnecting;
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
                                panels::s11_panel::receiver_fields(ui, &mut settings, language);
                                if settings.display == TraceDisplay::Spec {
                                    panels::spec_panel::receiver_fields(
                                        ui,
                                        &mut settings,
                                        language,
                                    );
                                }
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
                            trace.analysis.controls_for_display(
                                ui,
                                language,
                                complete.as_deref(),
                                trace.settings.display.is_smith(),
                            );
                            if trace.completed.is_some() && complete.is_none() {
                                ui.label(language.text(Text::RunForDisplay));
                            }
                            panels::workspace_panel::display_fields(ui, trace, language);
                        });
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
                                        egui::RichText::new(format!("T{}", trace.id.0)).strong(),
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
                                    visibility_control(ui, &mut trace.settings.visible, language);
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
                    })
            })
            .inner;
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
    use crate::state::WorkerCommand;
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
            temperature: Some(42.0),
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
            assert!(app.state.temperature.is_none());
            assert!(app.state.voltage.is_none());
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
                    host: "instrument.local".into(),
                    port: 901,
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
                WorkerEvent::Status {
                    temperature: 99.0,
                    voltage: kcsdi_core::data::Voltage {
                        external: 9.0,
                        battery: 7.0,
                    },
                },
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
                assert_eq!(app.state.temperature, Some(42.0));
                assert!(app.state.voltage.is_none());
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
        assert!(app.state.temperature.is_none());
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
            event: WorkerEvent::Status {
                temperature: 43.0,
                voltage: kcsdi_core::data::Voltage {
                    external: 12.0,
                    battery: 8.0,
                },
            },
        });
        assert_eq!(app.state.temperature, Some(43.0));
        assert!(app.state.any_running());
        assert_eq!(app.state.request_id, 3);
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
                            (Text::ExportS1p, false),
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
