// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Main application struct and eframe::App implementation.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::AppConfig;
use crate::desktop::{self, Page};
use crate::device_worker;
use crate::i18n::{self, Language, StatusMessage, Text};
use crate::panels;
use crate::state::{AppMode, AppState, ConnectionState, S11Display, WorkerEvent};
use crate::theme;
use crate::widgets;

/// Debounce before persisting config changes to disk.
const CONFIG_SAVE_DELAY: Duration = Duration::from_secs(1);

/// The main kcsdi GUI application.
pub struct KcsdiApp {
    /// Application state shared across all panels.
    pub state: AppState,
    /// Receiver for events from the device worker thread.
    evt_rx: mpsc::Receiver<WorkerEvent>,
    /// Last persisted config snapshot, for change detection.
    last_saved: AppConfig,
    /// When the first unsaved change happened (debounce start).
    dirty_since: Option<Instant>,
    connection_open: bool,
}

impl KcsdiApp {
    /// Create a new application instance.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::setup(&cc.egui_ctx);

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();

        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("device-worker".to_string())
            .spawn(move || {
                device_worker::device_worker(cmd_rx, evt_tx, ctx);
            })
            .expect("failed to spawn device worker thread");

        let cfg = crate::config::load();
        let mut state = AppState {
            cmd_tx: Some(cmd_tx),
            ..AppState::default()
        };
        cfg.apply_to(&mut state);
        state
            .spec
            .view
            .reset(state.spec.start_hz, state.spec.stop_hz, -100.0, 0.0);
        let (y_min, y_max) = state.s11.display.default_y();
        state
            .s11
            .view
            .reset(state.s11.start_hz, state.s11.stop_hz, y_min, y_max);

        Self {
            last_saved: AppConfig::from_state(&state),
            state,
            evt_rx,
            dirty_since: None,
            connection_open: false,
        }
    }

    /// Apply one worker event to the application state.
    fn apply_event(&mut self, evt: WorkerEvent) {
        match evt {
            WorkerEvent::Connected(info) => {
                self.connection_open = false;
                info!("connected, serial {}", info.serial);
                self.state.connection = ConnectionState::Connected;
                self.state.device_info = Some(info);
                self.state.status_message = Some(StatusMessage::Text(Text::Connected));
                self.state.send(crate::state::WorkerCommand::RefreshStatus);
            }
            WorkerEvent::Disconnected => {
                self.state.connection = ConnectionState::Disconnected;
                self.state.device_info = None;
                self.state.temperature = None;
                self.state.voltage = None;
                self.state.spec.running = false;
                self.state.s11.running = false;
                self.state.status_message = Some(StatusMessage::Text(Text::Disconnected));
            }
            WorkerEvent::Error(msg) => {
                if self.state.connection == ConnectionState::Connecting {
                    self.state.connection = ConnectionState::Error(msg.clone());
                }
                self.state.spec.running = false;
                self.state.s11.running = false;
                self.state.status_message = Some(msg.into());
            }
            WorkerEvent::SweepTrace(data) => {
                use kcsdi_core::protocol::StreamMode;
                match data.mode {
                    StreamMode::Spec => {
                        self.state.spec.analysis.observe(&data);
                        self.state.spec.needs_fit = true;
                        self.state.spec.trace = Some(data);
                    }
                    StreamMode::S11 => {
                        self.state.s11.analysis.observe(&data);
                        self.state.s11.needs_fit = true;
                        self.state.s11.trace = Some(data);
                    }
                    _ => {}
                }
                self.state.status_message = None;
            }
            WorkerEvent::Status {
                temperature,
                voltage,
            } => {
                self.state.temperature = Some(temperature);
                self.state.voltage = Some(voltage);
            }
        }
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
        S11Display::Smith => Vec::new(),
    }
}

impl eframe::App for KcsdiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        i18n::set_language(&ctx, self.state.language);
        theme::apply(&ctx, self.state.desktop.settings.theme);
        self.state.export.poll();

        // Drain all pending events from the device worker.
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.apply_event(evt);
        }

        if ctx.input(|input| input.key_pressed(egui::Key::F11)) {
            let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if self.state.desktop.page == Page::Instrument {
            self.instrument_ui(ui);
        } else {
            if self.state.connection == ConnectionState::Connected {
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

        ctx.request_repaint_after(Duration::from_millis(500));
        self.persist_config_debounced();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_config();
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
                    ui.menu_button(language.text(Text::Function), |ui| {
                        let mut mode = self.state.mode;
                        if ui
                            .selectable_value(&mut mode, AppMode::S11, "S11")
                            .clicked()
                        {
                            ui.close();
                        }
                        if ui
                            .selectable_value(
                                &mut mode,
                                AppMode::Spec,
                                language.text(Text::Spectrum),
                            )
                            .clicked()
                        {
                            ui.close();
                        }
                        self.state.change_mode(mode);
                    });
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
                        match self.state.mode {
                            AppMode::Spec => panels::spec_panel::show_sweep(ui, &mut self.state),
                            AppMode::S11 => panels::s11_panel::show_sweep(ui, &mut self.state),
                        }
                    });
            });
        parameter_panel(ui, &mut self.state);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui.visuals().panel_fill))
            .show(ui, |ui| self.plot_ui(ui));
    }

    fn plot_ui(&mut self, ui: &mut egui::Ui) {
        let colors = theme::trace_colors(ui.visuals().dark_mode);
        match self.state.mode {
            AppMode::Spec => {
                let spec = &mut self.state.spec;
                let mut series: Vec<_> = spec
                    .trace
                    .as_ref()
                    .map(|t| widgets::plot::Series {
                        name: self.state.language.text(Text::Level),
                        color: colors[1],
                        visible: spec.visible,
                        points: t
                            .points
                            .iter()
                            .map(|p| (p.freq_hz, p.values.first().copied().unwrap_or(f64::NAN)))
                            .collect(),
                    })
                    .into_iter()
                    .collect();
                let base_count = series.len();
                if spec.visible {
                    series.extend(spec.analysis.overlay_series(&[0]));
                }
                let mut opts = widgets::plot::PlotOptions {
                    y_label: "dBm",
                    log_x: spec.log_x,
                    series,
                };
                if spec.needs_fit && !spec.view_locked && spec.trace.is_some() {
                    widgets::plot::fit_view(&mut spec.view, &opts);
                    spec.needs_fit = false;
                }
                let interaction = if spec.analysis.markers().is_empty() {
                    widgets::plot::show(ui, &mut spec.view, &mut opts)
                } else {
                    widgets::plot::show_with_markers(
                        ui,
                        &mut spec.view,
                        &mut opts,
                        spec.analysis.markers_mut(),
                    )
                };
                match interaction {
                    widgets::plot::ViewLock::Locked => spec.view_locked = true,
                    widgets::plot::ViewLock::Unlocked => spec.view_locked = false,
                    widgets::plot::ViewLock::Unchanged => {}
                }
                if let Some(curve) = opts.series.first() {
                    spec.visible = curve.visible;
                }
                spec.analysis
                    .apply_overlay_visibility(&opts.series[base_count..]);
            }
            AppMode::S11 => {
                let s11 = &mut self.state.s11;
                match s11.display {
                    S11Display::Smith => {
                        let trace = s11.trace.as_ref().filter(|_| s11.visible);
                        let held = s11.visible.then(|| s11.analysis.held_trace()).flatten();
                        if held.is_some() {
                            widgets::smith::show_layers(
                                ui,
                                &mut s11.smith,
                                trace,
                                held.as_ref(),
                                s11.analysis.markers_mut(),
                            );
                        } else if s11.analysis.markers().is_empty() {
                            widgets::smith::show(ui, &mut s11.smith, trace);
                        } else {
                            widgets::smith::show_with_markers(
                                ui,
                                &mut s11.smith,
                                trace,
                                s11.analysis.markers_mut(),
                            );
                        }
                    }
                    display => {
                        let mut series = cartesian_series(
                            display,
                            s11.trace.as_ref(),
                            s11.impedance_visible,
                            self.state.language,
                        );
                        let base_count = series.len();
                        for (i, curve) in series.iter_mut().enumerate() {
                            curve.color = colors[i];
                            curve.visible &= s11.visible;
                        }
                        if s11.visible && base_count > 0 {
                            let columns: &[usize] = match display {
                                S11Display::Phase => &[1],
                                S11Display::Impedance => &[0, 1, 2],
                                _ => &[0],
                            };
                            series.extend(s11.analysis.overlay_series(columns));
                        }
                        let mut opts = widgets::plot::PlotOptions {
                            y_label: display.y_label(),
                            log_x: s11.log_x,
                            series,
                        };
                        if s11.needs_fit && !s11.view_locked && s11.trace.is_some() {
                            widgets::plot::fit_view(&mut s11.view, &opts);
                            s11.needs_fit = false;
                        }
                        match widgets::plot::show_with_markers(
                            ui,
                            &mut s11.view,
                            &mut opts,
                            s11.analysis.markers_mut(),
                        ) {
                            widgets::plot::ViewLock::Locked => s11.view_locked = true,
                            widgets::plot::ViewLock::Unlocked => s11.view_locked = false,
                            widgets::plot::ViewLock::Unchanged => {}
                        }
                        if display == S11Display::Impedance && base_count == 3 && s11.visible {
                            let visible = std::array::from_fn(|i| opts.series[i].visible);
                            if s11.impedance_visible != visible {
                                s11.impedance_visible = visible;
                                s11.needs_fit = true;
                                ui.ctx().request_repaint();
                            }
                        } else if base_count == 1 {
                            s11.visible = opts.series[0].visible;
                        }
                        s11.analysis
                            .apply_overlay_visibility(&opts.series[base_count..]);
                    }
                }
            }
        }
    }
}

pub(crate) fn parameter_panel(ui: &mut egui::Ui, state: &mut crate::state::AppState) {
    let language = state.language;
    egui::Panel::right("params_panel")
        .exact_size(256.0)
        .frame(
            egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .inner_margin(8),
        )
        .show(ui, |ui| {
            if state.mode == AppMode::S11 {
                egui::Panel::bottom("s11_export")
                    .default_size(128.0)
                    .min_size(112.0)
                    .show_separator_line(true)
                    .show(ui, |ui| {
                        crate::export::show(ui, &mut state.export, &state.s11, language);
                    });
            }
            egui::ScrollArea::vertical()
                .id_salt("analysis_scroll")
                .show(ui, |ui| match state.mode {
                    AppMode::Spec => {
                        panels::spec_panel::show(ui, state);
                        let spec = &mut state.spec;
                        spec.analysis.controls(ui, language, spec.trace.as_ref());
                        panels::spec_panel::show_display_controls(ui, state);
                    }
                    AppMode::S11 => {
                        panels::s11_panel::show(ui, state);
                        let s11 = &mut state.s11;
                        let trace = s11
                            .trace
                            .as_ref()
                            .filter(|trace| trace.format == s11.display.wire_format().as_str());
                        s11.analysis.controls_for_display(
                            ui,
                            language,
                            trace,
                            s11.display == S11Display::Smith,
                        );
                        panels::s11_panel::show_display_controls(ui, state);
                    }
                });
        });
}

fn trace_list(ui: &mut egui::Ui, state: &mut crate::state::AppState) {
    let language = state.language;
    ui.label(egui::RichText::new(language.text(Text::TraceList)).monospace())
        .on_hover_text(language.text(Text::SelectedTraceOnly));
    ui.add_space(4.0);
    for (index, mode) in [AppMode::S11, AppMode::Spec].into_iter().enumerate() {
        let selected = state.mode == mode;
        let color = theme::trace_colors(ui.visuals().dark_mode)[index];
        let fill = if selected {
            ui.visuals().faint_bg_color
        } else {
            ui.visuals().panel_fill
        };
        let card = egui::Frame::new()
            .inner_margin(8)
            .fill(fill)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(4)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.spacing_mut().interact_size.y = 18.0;
                ui.horizontal(|ui| {
                    let title = format!("T{}", index + 1);
                    if ui
                        .add(egui::Button::new(egui::RichText::new(title).strong()).frame(false))
                        .clicked()
                    {
                        state.change_mode(mode);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("...", |ui| {
                            ui.set_min_width(220.0);
                            ui.label(language.text(Text::TraceSettings));
                            if mode == AppMode::S11 {
                                panels::s11_panel::show_trace_editor(ui, state);
                            } else {
                                if widgets::plot::log_x_control(ui, &mut state.spec.log_x) {
                                    state.spec.needs_fit = true;
                                    state.spec.view_locked = false;
                                }
                            }
                        });
                        let visible = match mode {
                            AppMode::S11 => &mut state.s11.visible,
                            AppMode::Spec => &mut state.spec.visible,
                        };
                        visibility_control(ui, visible, language);
                    });
                });
                let label = match mode {
                    AppMode::S11 => format!("S11  {}", state.s11.display.label(language)),
                    AppMode::Spec => format!("{}  dBm", language.text(Text::Spectrum)),
                };
                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    state.change_mode(mode);
                }
            });
        if selected {
            ui.painter().line_segment(
                [
                    card.response.rect.left_top() + egui::vec2(1.0, 2.0),
                    card.response.rect.left_bottom() + egui::vec2(1.0, -2.0),
                ],
                egui::Stroke::new(3.0, color),
            );
        }
        ui.add_space(4.0);
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
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::protocol::StreamMode;

    fn test_app(state: crate::state::AppState) -> KcsdiApp {
        let (_, evt_rx) = mpsc::channel();
        KcsdiApp {
            last_saved: AppConfig::from_state(&state),
            state,
            evt_rx,
            dirty_since: None,
            connection_open: false,
        }
    }

    #[test]
    fn disconnect_clears_health_but_preserves_completed_export_data() {
        let mut state = crate::state::AppState::default();
        state.temperature = Some(42.0);
        state.voltage = Some(kcsdi_core::data::Voltage {
            external: 12.0,
            battery: 8.0,
        });
        state.s11.trace = Some(SweepData {
            mode: StreamMode::S11,
            format: "z".into(),
            points: vec![],
        });
        let mut app = test_app(state);
        app.apply_event(WorkerEvent::Disconnected);
        assert!(app.state.temperature.is_none());
        assert!(app.state.voltage.is_none());
        assert!(app.state.s11.trace.is_some());
    }

    #[test]
    fn completed_measurement_clears_the_request_to_run_again() {
        let mut state = crate::state::AppState::default();
        state.status_message = Some(StatusMessage::Text(Text::RunForDisplay));
        let mut app = test_app(state);
        app.apply_event(WorkerEvent::SweepTrace(SweepData {
            mode: StreamMode::S11,
            format: "z".into(),
            points: vec![],
        }));
        assert!(app.state.status_message.is_none());
        assert!(app.state.s11.trace.is_some());
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
                        mode: AppMode::S11,
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
        let mut state = crate::state::S11State::default();
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
