// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Main application struct and eframe::App implementation.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::AppConfig;
use crate::device_worker;
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
                self.state.status_message = Some("Connected".to_string());
                self.state.send(crate::state::WorkerCommand::RefreshStatus);
            }
            WorkerEvent::Disconnected => {
                self.state.connection = ConnectionState::Disconnected;
                self.state.device_info = None;
                self.state.spec.running = false;
                self.state.s11.running = false;
                self.state.status_message = Some("Disconnected".to_string());
            }
            WorkerEvent::Error(msg) => {
                if self.state.connection == ConnectionState::Connecting {
                    self.state.connection = ConnectionState::Error(msg.clone());
                }
                self.state.spec.running = false;
                self.state.s11.running = false;
                self.state.status_message = Some(msg);
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
) -> Vec<widgets::plot::Series<'static>> {
    let Some(trace) = trace else {
        return Vec::new();
    };
    let column = |name: &'static str, i: usize, color: egui::Color32| widgets::plot::Series {
        name,
        color,
        points: trace
            .points
            .iter()
            .map(|p| (p.freq_hz, p.values.get(i).copied().unwrap_or(f64::NAN)))
            .collect(),
    };
    match display {
        S11Display::Phase => vec![column("Phase", 1, theme::TRACE_COLORS[0])],
        S11Display::ReturnLoss => vec![column("RL", 0, theme::TRACE_COLORS[0])],
        S11Display::Vswr => vec![column("VSWR", 0, theme::TRACE_COLORS[0])],
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

        // Drain all pending events from the device worker.
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.apply_event(evt);
        }

        egui::Panel::top("top_bar").show(ui, |ui| {
            panels::top_bar::show(ui, &mut self.state);
        });
        egui::Panel::top("mode_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.state.mode, AppMode::Spec, "SPEC");
                ui.selectable_value(&mut self.state.mode, AppMode::S11, "S11");
            });
        });
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
                        name: "Level",
                        color: theme::TRACE_COLORS[0],
                        points: t
                            .points
                            .iter()
                            .map(|p| (p.freq_hz, p.values.first().copied().unwrap_or(f64::NAN)))
                            .collect(),
                    })
                    .into_iter()
                    .collect();
                let opts = widgets::plot::PlotOptions {
                    y_label: "dBm",
                    log_y: false,
                    series,
                };
                if spec.needs_fit && !spec.view_locked && spec.trace.is_some() {
                    widgets::plot::fit_view(&mut spec.view, &opts);
                    spec.needs_fit = false;
                }
                match widgets::plot::show(ui, &mut spec.view, &opts) {
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
                        let series = cartesian_series(display, s11.trace.as_ref());
                        let opts = widgets::plot::PlotOptions {
                            y_label: display.y_label(),
                            log_y: s11.log_y,
                            series,
                        };
                        if s11.needs_fit && !s11.view_locked && s11.trace.is_some() {
                            widgets::plot::fit_view(&mut s11.view, &opts);
                            s11.needs_fit = false;
                        }
                        match widgets::plot::show(ui, &mut s11.view, &opts) {
                            widgets::plot::ViewLock::Locked => s11.view_locked = true,
                            widgets::plot::ViewLock::Unlocked => s11.view_locked = false,
                            widgets::plot::ViewLock::Unchanged => {}
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
