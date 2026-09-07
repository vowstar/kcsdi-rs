// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Main application struct and eframe::App implementation.

use std::sync::mpsc;

use log::info;

use crate::device_worker;
use crate::panels;
use crate::state::{AppState, ConnectionState, WorkerEvent};
use crate::theme;
use crate::widgets;

/// The main kcsdi GUI application.
pub struct KcsdiApp {
    /// Application state shared across all panels.
    pub state: AppState,
    /// Receiver for events from the device worker thread.
    evt_rx: mpsc::Receiver<WorkerEvent>,
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

        Self {
            state: AppState {
                cmd_tx: Some(cmd_tx),
                ..AppState::default()
            },
            evt_rx,
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
                self.state.status_message = Some("Disconnected".to_string());
            }
            WorkerEvent::Error(msg) => {
                if self.state.connection == ConnectionState::Connecting {
                    self.state.connection = ConnectionState::Error(msg.clone());
                }
                self.state.spec.running = false;
                self.state.status_message = Some(msg);
            }
            WorkerEvent::SpecTrace(data) => {
                self.state.spec.trace = Some(data);
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
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            panels::status_bar::show(ui, &mut self.state);
        });
        egui::Panel::right("spec_panel")
            .default_size(260.0)
            .resizable(true)
            .show(ui, |ui| {
                panels::spec_panel::show(ui, &mut self.state);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            let spec = &mut self.state.spec;
            widgets::plot::show(ui, &mut spec.view, spec.trace.as_ref());
        });

        // Pick up worker events promptly even when idle.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}
