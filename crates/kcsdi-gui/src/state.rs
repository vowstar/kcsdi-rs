// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Central application state shared by all panels.
//!
//! Every panel is a free function taking `&mut AppState`; there is no
//! global or interior-mutable state outside this struct.

use std::sync::mpsc;

use kcsdi_core::data::{DeviceInfo, SweepData, Voltage};
use kcsdi_core::device::SpecParams;
use kcsdi_core::model::Rbw;

use crate::widgets::plot::PlotView;

/// Commands sent from the UI to the device worker thread.
#[derive(Debug)]
pub enum WorkerCommand {
    /// Connect to an instrument over TCP and perform the handshake.
    Connect { host: String, port: u16 },
    /// Drop remote mode and close the connection.
    Disconnect,
    /// Start repeating SPEC sweeps with these parameters.
    RunSpec(SpecParams),
    /// Stop repeating sweeps.
    StopSpec,
    /// One-shot temperature/voltage refresh.
    RefreshStatus,
}

/// Events sent from the device worker thread to the UI.
#[derive(Debug)]
pub enum WorkerEvent {
    /// Handshake done and identity read.
    Connected(DeviceInfo),
    /// Connection closed (requested or lost).
    Disconnected,
    /// Any worker-side failure, already formatted for display.
    Error(String),
    /// A completed SPEC sweep.
    SpecTrace(SweepData),
    /// Temperature and voltage reading.
    Status { temperature: f64, voltage: Voltage },
}

/// Connection lifecycle shown in the top bar.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// SPEC mode state: sweep parameters, latest trace, and plot viewport.
pub struct SpecState {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub center_hz: f64,
    pub span_hz: f64,
    pub points: u32,
    pub rbw: Rbw,
    pub ref_level_dbm: i32,
    /// True while repeating sweeps are requested.
    pub running: bool,
    /// Latest completed sweep.
    pub trace: Option<SweepData>,
    /// Plot viewport (frequency x level), owned by the plot widget.
    pub view: PlotView,
}

impl Default for SpecState {
    fn default() -> Self {
        let start_hz = 100e6;
        let stop_hz = 500e6;
        Self {
            start_hz,
            stop_hz,
            center_hz: (start_hz + stop_hz) / 2.0,
            span_hz: stop_hz - start_hz,
            points: 201,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
            running: false,
            trace: None,
            view: PlotView::new(start_hz, stop_hz, -100.0, 0.0),
        }
    }
}

impl SpecState {
    /// Recompute center/span after start or stop changed.
    pub fn start_stop_changed(&mut self) {
        if self.stop_hz < self.start_hz {
            std::mem::swap(&mut self.start_hz, &mut self.stop_hz);
        }
        self.center_hz = (self.start_hz + self.stop_hz) / 2.0;
        self.span_hz = self.stop_hz - self.start_hz;
    }

    /// Recompute start/stop after center or span changed.
    pub fn center_span_changed(&mut self) {
        if self.span_hz < 0.0 {
            self.span_hz = -self.span_hz;
        }
        self.start_hz = self.center_hz - self.span_hz / 2.0;
        self.stop_hz = self.center_hz + self.span_hz / 2.0;
    }

    /// Build worker parameters from the current field values.
    pub fn spec_params(&self) -> SpecParams {
        SpecParams {
            cal: kcsdi_core::commands::Cal::CalOff,
            lo: kcsdi_core::commands::Lo::HighLo,
            points: self.points,
            start_hz: self.start_hz as u64,
            stop_hz: self.stop_hz as u64,
            rbw: self.rbw,
            ref_level_dbm: self.ref_level_dbm,
        }
    }
}

/// Application state shared across all panels.
pub struct AppState {
    /// Host field of the connection bar.
    pub host: String,
    /// Port field of the connection bar.
    pub port: u16,
    pub connection: ConnectionState,
    /// Identity packet of the connected instrument.
    pub device_info: Option<DeviceInfo>,
    pub temperature: Option<f64>,
    pub voltage: Option<Voltage>,
    pub spec: SpecState,
    /// Transient message for the status bar.
    pub status_message: Option<String>,
    /// Command channel to the device worker thread.
    pub cmd_tx: Option<mpsc::Sender<WorkerCommand>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 901,
            connection: ConnectionState::Disconnected,
            device_info: None,
            temperature: None,
            voltage: None,
            spec: SpecState::default(),
            status_message: None,
            cmd_tx: None,
        }
    }
}

impl AppState {
    /// Send a command to the device worker, dropping it silently when the
    /// worker is gone (it outlives no panic).
    pub fn send(&self, cmd: WorkerCommand) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }
}
