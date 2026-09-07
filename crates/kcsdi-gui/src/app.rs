// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Main application struct and eframe::App implementation.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::AppConfig;
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

        Self {
            last_saved: AppConfig::from_state(&state),
            state,
            evt_rx,
            dirty_since: None,
        }
    }

    /// Apply one worker event to the application state.
    fn apply_event(&mut self, evt: WorkerEvent) {
        match evt {
            WorkerEvent::Connected(info) => {
                info!("connected, serial {}", info.serial);
                self.state.connection = ConnectionState::Connected;
                self.state.device_info = Some(info);
                self.state.status_message = Some(StatusMessage::Text(Text::Connected));
                self.state.send(crate::state::WorkerCommand::RefreshStatus);
            }
            WorkerEvent::Disconnected => {
                self.state.connection = ConnectionState::Disconnected;
                self.state.device_info = None;
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
                        self.state.spec.needs_fit = true;
                        self.state.spec.trace = Some(data);
                    }
                    StreamMode::S11 => {
                        self.state.s11.needs_fit = true;
                        self.state.s11.trace = Some(data);
                    }
                    _ => {}
                }
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

/// The selector remains available while disconnected or scanning.
fn language_selector(ui: &mut egui::Ui, language: &mut Language) {
    egui::ComboBox::from_id_salt("language_selector")
        .selected_text(language.label())
        .show_ui(ui, |ui| {
            for choice in Language::ALL {
                ui.selectable_value(language, choice, choice.label());
            }
        });
    ui.label(language.text(Text::Language));
    i18n::set_language(ui.ctx(), *language);
}

impl eframe::App for KcsdiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        i18n::set_language(&ctx, self.state.language);
        self.state.export.poll();

        // Drain all pending events from the device worker.
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.apply_event(evt);
        }

        egui::Panel::top("top_bar").show(ui, |ui| {
            panels::top_bar::show(ui, &mut self.state);
        });
        let mut mode = self.state.mode;
        egui::Panel::top("mode_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut mode,
                    AppMode::Spec,
                    self.state.language.text(Text::Spectrum),
                );
                ui.selectable_value(&mut mode, AppMode::S11, "S11");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    language_selector(ui, &mut self.state.language);
                });
            });
        });
        self.state.change_mode(mode);
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            panels::status_bar::show(ui, &mut self.state);
        });
        egui::Panel::right("params_panel")
            .default_size(260.0)
            .resizable(true)
            .show(ui, |ui| match self.state.mode {
                AppMode::Spec => panels::spec_panel::show(ui, &mut self.state),
                AppMode::S11 => panels::s11_panel::show(ui, &mut self.state),
            });
        egui::CentralPanel::default().show(ui, |ui| match self.state.mode {
            AppMode::Spec => {
                let spec = &mut self.state.spec;
                let series = spec
                    .trace
                    .as_ref()
                    .map(|t| widgets::plot::Series {
                        name: self.state.language.text(Text::Level),
                        color: theme::TRACE_COLORS[0],
                        visible: true,
                        points: t
                            .points
                            .iter()
                            .map(|p| (p.freq_hz, p.values.first().copied().unwrap_or(f64::NAN)))
                            .collect(),
                    })
                    .into_iter()
                    .collect();
                let mut opts = widgets::plot::PlotOptions {
                    y_label: "dBm",
                    log_x: spec.log_x,
                    series,
                };
                if spec.needs_fit && !spec.view_locked && spec.trace.is_some() {
                    widgets::plot::fit_view(&mut spec.view, &opts);
                    spec.needs_fit = false;
                }
                match widgets::plot::show(ui, &mut spec.view, &mut opts) {
                    widgets::plot::ViewLock::Locked => spec.view_locked = true,
                    widgets::plot::ViewLock::Unlocked => spec.view_locked = false,
                    widgets::plot::ViewLock::Unchanged => {}
                }
            }
            AppMode::S11 => {
                let s11 = &mut self.state.s11;
                match s11.display {
                    S11Display::Smith => {
                        widgets::smith::show(ui, &mut s11.smith, s11.trace.as_ref());
                    }
                    display => {
                        let series = cartesian_series(
                            display,
                            s11.trace.as_ref(),
                            s11.impedance_visible,
                            self.state.language,
                        );
                        let mut opts = widgets::plot::PlotOptions {
                            y_label: display.y_label(),
                            log_x: s11.log_x,
                            series,
                        };
                        if s11.needs_fit && !s11.view_locked && s11.trace.is_some() {
                            widgets::plot::fit_view(&mut s11.view, &opts);
                            s11.needs_fit = false;
                        }
                        match widgets::plot::show(ui, &mut s11.view, &mut opts) {
                            widgets::plot::ViewLock::Locked => s11.view_locked = true,
                            widgets::plot::ViewLock::Unlocked => s11.view_locked = false,
                            widgets::plot::ViewLock::Unchanged => {}
                        }
                        if display == S11Display::Impedance && opts.series.len() == 3 {
                            let visible = std::array::from_fn(|i| opts.series[i].visible);
                            if s11.impedance_visible != visible {
                                s11.impedance_visible = visible;
                                s11.needs_fit = true;
                                ctx.request_repaint();
                            }
                        }
                    }
                }
            }
        });

        // Pick up worker events promptly even when idle.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
        self.persist_config_debounced();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_config();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::protocol::StreamMode;

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
