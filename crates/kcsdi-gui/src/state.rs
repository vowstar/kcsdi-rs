// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Central application state shared by all panels.
//!
//! Every panel is a free function taking `&mut AppState`; there is no
//! global or interior-mutable state outside this struct.

use std::sync::mpsc;

use kcsdi_core::commands::{Cal, Format};
use kcsdi_core::data::{DeviceInfo, SweepData, Voltage};
use kcsdi_core::device::{S11Params, SpecParams};
use kcsdi_core::model::Rbw;

use crate::widgets::plot::PlotView;
use crate::widgets::smith::SmithView;

/// Commands sent from the UI to the device worker thread.
#[derive(Debug)]
pub enum WorkerCommand {
    /// Connect to an instrument over TCP and perform the handshake.
    Connect { host: String, port: u16 },
    /// Drop remote mode and close the connection.
    Disconnect,
    /// Start repeating SPEC sweeps with these parameters.
    RunSpec(SpecParams),
    /// Start repeating S11 sweeps with these parameters.
    RunS11(S11Params),
    /// Stop repeating sweeps.
    StopSweep,
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
    /// A completed sweep (SPEC or S11; see `SweepData::mode`).
    SweepTrace(SweepData),
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

/// Top-level instrument function mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppMode {
    #[default]
    Spec,
    S11,
}

/// S11 display formats, matching the reference interface tabs
/// (protocol reference 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum S11Display {
    /// Phase of S11 in degrees (wire `ma`, column 2).
    Phase,
    /// Return loss in dB (wire `loss`, column 1).
    #[default]
    ReturnLoss,
    /// Voltage standing wave ratio (wire `vswr`, column 1).
    Vswr,
    /// Smith chart (wire `z`, columns 2-3 as R, X).
    Smith,
    /// Impedance |Z|, R, X vs frequency (wire `z`, columns 1-3).
    Impedance,
}

impl S11Display {
    pub const ALL: [S11Display; 5] = [
        Self::Phase,
        Self::ReturnLoss,
        Self::Vswr,
        Self::Smith,
        Self::Impedance,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Phase => "Phase",
            Self::ReturnLoss => "Return Loss",
            Self::Vswr => "VSWR",
            Self::Smith => "Smith",
            Self::Impedance => "Impedance",
        }
    }

    /// Wire format sent in the `run` command.
    pub fn wire_format(self) -> Format {
        match self {
            Self::Phase => Format::Ma,
            Self::ReturnLoss => Format::Loss,
            Self::Vswr => Format::Vswr,
            Self::Smith | Self::Impedance => Format::Z,
        }
    }

    /// Default Y axis range for a fresh view.
    pub fn default_y(self) -> (f64, f64) {
        match self {
            Self::Phase => (-180.0, 180.0),
            Self::ReturnLoss => (0.0, 50.0),
            Self::Vswr => (1.0, 10.0),
            Self::Smith => (-1.0, 1.0),
            Self::Impedance => (0.0, 200.0),
        }
    }

    /// Y axis unit label.
    pub fn y_label(self) -> &'static str {
        match self {
            Self::Phase => "deg",
            Self::ReturnLoss => "dB",
            Self::Vswr => "",
            Self::Smith => "",
            Self::Impedance => "ohm",
        }
    }
}

/// Linked start/stop/center/span logic shared by SPEC and S11 states.
macro_rules! impl_freq_helpers {
    () => {
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
    };
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
    /// Logarithmic frequency axis for the spectrum plot.
    pub log_x: bool,
    /// True while repeating sweeps are requested.
    pub running: bool,
    /// Latest completed sweep.
    pub trace: Option<SweepData>,
    /// Plot viewport (frequency x level), owned by the plot widget.
    pub view: PlotView,
    /// Fit the view to the next trace (set on run and on new data).
    pub needs_fit: bool,
    /// User adjusted the view manually; auto-fit stays off.
    pub view_locked: bool,
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
            log_x: false,
            running: false,
            trace: None,
            view: PlotView::new(start_hz, stop_hz, -100.0, 0.0),
            needs_fit: true,
            view_locked: false,
        }
    }
}

impl SpecState {
    impl_freq_helpers!();

    /// Build worker parameters from the current field values.
    pub fn spec_params(&self) -> SpecParams {
        SpecParams {
            cal: Cal::CalOff,
            lo: kcsdi_core::commands::Lo::HighLo,
            points: self.points,
            start_hz: self.start_hz as u64,
            stop_hz: self.stop_hz as u64,
            rbw: self.rbw,
            ref_level_dbm: self.ref_level_dbm,
        }
    }
}

/// S11 mode state: sweep parameters, display format, and latest trace.
pub struct S11State {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub center_hz: f64,
    pub span_hz: f64,
    pub points: u32,
    pub cal: Cal,
    pub display: S11Display,
    /// Logarithmic frequency axis for cartesian displays.
    pub log_x: bool,
    /// Optional RBW pushed before the run (`$bw`).
    pub rbw: Option<Rbw>,
    /// True while repeating sweeps are requested.
    pub running: bool,
    /// Latest completed sweep.
    pub trace: Option<SweepData>,
    /// Cartesian plot viewport.
    pub view: PlotView,
    /// Smith chart viewport.
    pub smith: SmithView,
    /// Fit the view to the next trace (set on run, display change, and
    /// new data).
    pub needs_fit: bool,
    /// User adjusted the view manually; auto-fit stays off.
    pub view_locked: bool,
}

impl Default for S11State {
    fn default() -> Self {
        let start_hz = 1e6;
        let stop_hz = 1000e6;
        let display = S11Display::default();
        let (y_min, y_max) = display.default_y();
        Self {
            start_hz,
            stop_hz,
            center_hz: (start_hz + stop_hz) / 2.0,
            span_hz: stop_hz - start_hz,
            points: 201,
            cal: Cal::CalOff,
            display,
            log_x: false,
            rbw: None,
            running: false,
            trace: None,
            view: PlotView::new(start_hz, stop_hz, y_min, y_max),
            smith: SmithView::default(),
            needs_fit: true,
            view_locked: false,
        }
    }
}

impl S11State {
    impl_freq_helpers!();

    /// Build worker parameters from the current field values.
    pub fn s11_params(&self) -> S11Params {
        S11Params {
            cal: self.cal,
            format: self.display.wire_format(),
            points: self.points,
            start_hz: self.start_hz as u64,
            stop_hz: self.stop_hz as u64,
            rbw: self.rbw,
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
    /// Current function mode.
    pub mode: AppMode,
    pub spec: SpecState,
    pub s11: S11State,
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
            mode: AppMode::Spec,
            spec: SpecState::default(),
            s11: S11State::default(),
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

    /// Whether any sweep is currently repeating.
    pub fn any_running(&self) -> bool {
        self.spec.running || self.s11.running
    }
}
