// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Central application state shared by all panels.
//!
//! Every panel is a free function taking `&mut AppState`; there is no
//! global or interior-mutable state outside this struct.

use std::sync::mpsc;

use kcsdi_core::commands::{Cal, Format};
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::{DeviceInfo, SweepData, Voltage};
use kcsdi_core::device::{S11Params, SpecParams};
use kcsdi_core::model::{Model, Rbw};
use kcsdi_core::validation::frequency_hz;

use crate::i18n::{Language, StatusMessage, Text};
use crate::preview::{PreviewEnvelope, PreviewMailbox};
use crate::widgets::plot::PlotView;
use crate::widgets::smith::SmithView;

/// The GUI currently targets KC901V. Identity packets do not identify a
/// model reliably, so this must match the worker's explicit session model.
pub const DEVICE_MODEL: Model = Model::Kc901V;

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
    /// Release the device and terminate its worker.
    Shutdown,
}

/// Events sent from the device worker thread to the UI.
#[derive(Debug)]
pub enum WorkerEvent {
    /// Handshake done and identity read.
    Connected(DeviceInfo),
    /// Connection closed on request.
    Disconnected,
    /// The connection cannot safely be reused after a transport failure.
    ConnectionLost(String),
    /// Any worker-side failure, already formatted for display.
    Error(String),
    /// A completed sweep (SPEC or S11; see `SweepData::mode`).
    SweepTrace(SweepData),
    /// The requested stop has finished on the worker.
    SweepStopped,
    /// Temperature and voltage reading.
    Status { temperature: f64, voltage: Voltage },
}

/// Identity of the connection and measurement requested by a command.
#[derive(Debug)]
pub struct CommandEnvelope {
    pub session_id: u64,
    pub request_id: u64,
    pub cancel: CancellationToken,
    pub command: WorkerCommand,
}

/// Identity of the operation that produced a worker event.
#[derive(Debug)]
pub struct EventEnvelope {
    pub session_id: u64,
    pub request_id: u64,
    pub cycle_id: Option<u64>,
    pub event: WorkerEvent,
}

/// Connection lifecycle shown in the top bar.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error(String),
}

/// Top-level instrument function mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppMode {
    #[default]
    Spec,
    S11,
}

/// One acquisition belongs to the worker, regardless of the visible panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SweepState {
    #[default]
    Idle,
    Running(AppMode),
    Stopping,
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

    pub fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Phase => Text::Phase,
            Self::ReturnLoss => Text::ReturnLoss,
            Self::Vswr => Text::Vswr,
            Self::Smith => Text::Smith,
            Self::Impedance => Text::Impedance,
        })
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
            self.center_hz = (self.start_hz + self.stop_hz) / 2.0;
            self.span_hz = self.stop_hz - self.start_hz;
        }

        /// Recompute start/stop after center or span changed.
        pub fn center_span_changed(&mut self) {
            self.start_hz = self.center_hz - self.span_hz / 2.0;
            self.stop_hz = self.center_hz + self.span_hz / 2.0;
        }
    };
}

/// SPEC mode state: sweep parameters, latest trace, and plot viewport.
pub struct SpecState {
    pub visible: bool,
    pub analysis: crate::analysis_tools::AnalysisTools,
    pub start_hz: f64,
    pub stop_hz: f64,
    pub center_hz: f64,
    pub span_hz: f64,
    pub points: u32,
    pub rbw: Rbw,
    pub ref_level_dbm: i32,
    /// Logarithmic frequency axis for the spectrum plot.
    pub log_x: bool,
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
            visible: true,
            analysis: crate::analysis_tools::AnalysisTools::default(),
            start_hz,
            stop_hz,
            center_hz: (start_hz + stop_hz) / 2.0,
            span_hz: stop_hz - start_hz,
            points: 201,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
            log_x: false,
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
    pub fn spec_params(&self) -> kcsdi_core::Result<SpecParams> {
        let params = SpecParams {
            cal: Cal::CalOff,
            lo: kcsdi_core::commands::Lo::HighLo,
            points: self.points,
            start_hz: frequency_hz(self.start_hz, "SPEC start")?,
            stop_hz: frequency_hz(self.stop_hz, "SPEC stop")?,
            rbw: self.rbw,
            ref_level_dbm: self.ref_level_dbm,
        };
        params.validate(&DEVICE_MODEL.capabilities())?;
        Ok(params)
    }
}

/// S11 mode state: sweep parameters, display format, and latest trace.
pub struct S11State {
    pub visible: bool,
    pub analysis: crate::analysis_tools::AnalysisTools,
    pub start_hz: f64,
    pub stop_hz: f64,
    pub center_hz: f64,
    pub span_hz: f64,
    pub points: u32,
    pub cal: Cal,
    pub display: S11Display,
    /// Logarithmic frequency axis for cartesian displays.
    pub log_x: bool,
    /// Session-only visibility for |Z|, R, X. New sweeps keep this choice.
    pub impedance_visible: [bool; 3],
    /// Optional RBW pushed before the run (`$bw`).
    pub rbw: Option<Rbw>,
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
            visible: true,
            analysis: crate::analysis_tools::AnalysisTools::default(),
            start_hz,
            stop_hz,
            center_hz: (start_hz + stop_hz) / 2.0,
            span_hz: stop_hz - start_hz,
            points: 201,
            cal: Cal::CalOff,
            display,
            log_x: false,
            impedance_visible: [true; 3],
            rbw: None,
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
    pub fn s11_params(&self) -> kcsdi_core::Result<S11Params> {
        let params = S11Params {
            cal: self.cal,
            format: self.display.wire_format(),
            points: self.points,
            start_hz: frequency_hz(self.start_hz, "S11 start")?,
            stop_hz: frequency_hz(self.stop_hz, "S11 stop")?,
            rbw: self.rbw,
        };
        params.validate(&DEVICE_MODEL.capabilities())?;
        Ok(params)
    }
}

/// Application state shared across all panels.
pub struct AppState {
    pub desktop: crate::desktop::DesktopState,
    /// Language for labels and UI messages, independent of protocol data.
    pub language: Language,
    pub language_preference: crate::i18n::LanguagePreference,
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
    /// Export jobs never share the instrument command channel.
    pub export: crate::export::ExportState,
    /// Transient message for the status bar.
    pub status_message: Option<StatusMessage>,
    /// Session-only identities. Settings never restore an active request.
    pub session_id: u64,
    pub request_id: u64,
    pub sweep: SweepState,
    pub session_cancel: CancellationToken,
    pub acquisition_cancel: CancellationToken,
    pub worker_shutdown: CancellationToken,
    /// Preview data never replaces completed snapshots used by analysis/export.
    pub preview: Option<PreviewEnvelope>,
    pub preview_mailbox: PreviewMailbox,
    pub last_completed_cycle: Option<u64>,
    /// Command channel to the device worker thread.
    pub cmd_tx: Option<mpsc::Sender<CommandEnvelope>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            desktop: crate::desktop::DesktopState::default(),
            language: Language::default(),
            language_preference: crate::i18n::LanguagePreference::default(),
            host: String::new(),
            port: 901,
            connection: ConnectionState::Disconnected,
            device_info: None,
            temperature: None,
            voltage: None,
            mode: AppMode::Spec,
            spec: SpecState::default(),
            s11: S11State::default(),
            export: crate::export::ExportState::default(),
            status_message: None,
            session_id: 0,
            request_id: 0,
            sweep: SweepState::Idle,
            session_cancel: CancellationToken::default(),
            acquisition_cancel: CancellationToken::default(),
            worker_shutdown: CancellationToken::default(),
            preview: None,
            preview_mailbox: PreviewMailbox::default(),
            last_completed_cycle: None,
            cmd_tx: None,
        }
    }
}

impl AppState {
    pub fn set_language_preference(&mut self, preference: crate::i18n::LanguagePreference) {
        self.language_preference = preference;
        self.language = preference.resolve();
    }

    /// Switching panels stops the old job and keeps both RUN indicators
    /// consistent with the worker's single active measurement mode.
    pub fn change_mode(&mut self, mode: AppMode) {
        if self.mode != mode {
            if self.any_running() {
                self.send(WorkerCommand::StopSweep);
            }
            self.mode = mode;
        }
    }

    /// Invalidate prior results before a new operation enters the worker queue.
    pub fn send(&mut self, command: WorkerCommand) {
        if matches!(
            command,
            WorkerCommand::Connect { .. } | WorkerCommand::Disconnect | WorkerCommand::Shutdown
        ) {
            self.session_cancel.cancel();
            self.acquisition_cancel.cancel();
            self.session_cancel = CancellationToken::default();
            self.session_id = self
                .session_id
                .checked_add(1)
                .expect("session ID exhausted");
        }
        if !matches!(command, WorkerCommand::RefreshStatus) {
            self.clear_preview();
            self.request_id = self
                .request_id
                .checked_add(1)
                .expect("request ID exhausted");
        }
        let cancel = match &command {
            WorkerCommand::RunSpec(_) | WorkerCommand::RunS11(_) => {
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.sweep = SweepState::Running(if matches!(command, WorkerCommand::RunSpec(_)) {
                    AppMode::Spec
                } else {
                    AppMode::S11
                });
                self.acquisition_cancel.clone()
            }
            WorkerCommand::StopSweep => {
                self.acquisition_cancel.cancel();
                self.sweep = SweepState::Stopping;
                CancellationToken::default()
            }
            WorkerCommand::Connect { .. } => {
                self.sweep = SweepState::Idle;
                self.connection = ConnectionState::Connecting;
                self.session_cancel.clone()
            }
            WorkerCommand::Disconnect | WorkerCommand::Shutdown => {
                self.sweep = SweepState::Idle;
                self.connection = ConnectionState::Disconnecting;
                if matches!(command, WorkerCommand::Shutdown) {
                    self.worker_shutdown.cancel();
                }
                self.session_cancel.clone()
            }
            WorkerCommand::RefreshStatus => self.session_cancel.clone(),
        };
        if let Some(tx) = &self.cmd_tx
            && tx
                .send(CommandEnvelope {
                    session_id: self.session_id,
                    request_id: self.request_id,
                    cancel,
                    command,
                })
                .is_err()
        {
            self.worker_stopped();
        }
    }

    /// Whether any sweep is currently repeating.
    pub fn any_running(&self) -> bool {
        matches!(self.sweep, SweepState::Running(_))
    }

    pub fn running(&self, mode: AppMode) -> bool {
        self.sweep == SweepState::Running(mode)
    }

    pub fn sweep_busy(&self) -> bool {
        self.sweep != SweepState::Idle
    }

    pub fn clear_preview(&mut self) {
        self.preview = None;
        self.preview_mailbox.clear();
        self.last_completed_cycle = None;
    }

    pub fn worker_stopped(&mut self) {
        self.session_cancel.cancel();
        self.acquisition_cancel.cancel();
        self.worker_shutdown.cancel();
        self.sweep = SweepState::Idle;
        self.clear_preview();
        self.device_info = None;
        self.temperature = None;
        self.voltage = None;
        let message = "Device worker stopped".to_string();
        self.connection = ConnectionState::Error(message.clone());
        self.status_message = Some(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_envelopes_advance_only_the_relevant_identity() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        for (command, expected) in [
            (
                WorkerCommand::Connect {
                    host: "instrument.local".into(),
                    port: 901,
                },
                (1, 1),
            ),
            (WorkerCommand::RefreshStatus, (1, 1)),
            (
                WorkerCommand::RunSpec(SpecState::default().spec_params().unwrap()),
                (1, 2),
            ),
            (
                WorkerCommand::RunS11(S11State::default().s11_params().unwrap()),
                (1, 3),
            ),
            (WorkerCommand::RefreshStatus, (1, 3)),
            (WorkerCommand::StopSweep, (1, 4)),
            (WorkerCommand::Disconnect, (2, 5)),
            (
                WorkerCommand::Connect {
                    host: "instrument.local".into(),
                    port: 901,
                },
                (3, 6),
            ),
        ] {
            state.send(command);
            let envelope = rx.try_recv().unwrap();
            assert_eq!((state.session_id, state.request_id), expected);
            assert_eq!((envelope.session_id, envelope.request_id), expected);
        }
    }

    #[test]
    fn acquisition_and_session_cancellation_have_separate_lifetimes() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.send(WorkerCommand::Connect {
            host: "instrument.local".into(),
            port: 901,
        });
        let connect = rx.try_recv().unwrap();
        state.send(WorkerCommand::RunS11(state.s11.s11_params().unwrap()));
        let first = rx.try_recv().unwrap();
        state.send(WorkerCommand::RefreshStatus);
        let refresh = rx.try_recv().unwrap();
        assert_eq!(refresh.request_id, first.request_id);
        assert!(!first.cancel.is_cancelled());
        state.send(WorkerCommand::RunS11(state.s11.s11_params().unwrap()));
        let second = rx.try_recv().unwrap();
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled());
        assert!(!connect.cancel.is_cancelled());
        assert!(!refresh.cancel.is_cancelled());
        state.send(WorkerCommand::StopSweep);
        let stop = rx.try_recv().unwrap();
        assert!(second.cancel.is_cancelled());
        assert!(!stop.cancel.is_cancelled());
        assert!(!connect.cancel.is_cancelled());
        assert!(!state.worker_shutdown.is_cancelled());
        assert_eq!(state.sweep, SweepState::Stopping);
        state.send(WorkerCommand::Disconnect);
        assert!(connect.cancel.is_cancelled());
        assert!(refresh.cancel.is_cancelled());
        assert_eq!(state.connection, ConnectionState::Disconnecting);
        assert!(!state.worker_shutdown.is_cancelled());
        state.send(WorkerCommand::Shutdown);
        assert!(state.worker_shutdown.is_cancelled());
    }

    #[test]
    fn a_closed_command_channel_cannot_leave_a_run_indicator_active() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.send(WorkerCommand::RunSpec(state.spec.spec_params().unwrap()));
        assert_eq!(state.sweep, SweepState::Idle);
        assert!(matches!(state.connection, ConnectionState::Error(_)));
        assert!(state.worker_shutdown.is_cancelled());
    }

    #[test]
    fn changing_mode_waits_for_the_old_job_to_stop() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..AppState::default()
        };
        state.sweep = SweepState::Running(AppMode::Spec);
        state.change_mode(AppMode::S11);
        assert_eq!(state.mode, AppMode::S11);
        assert!(!state.any_running());
        assert_eq!(state.sweep, SweepState::Stopping);
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::StopSweep
        ));
        state.change_mode(AppMode::S11);
        assert!(rx.try_recv().is_err());
        state.sweep = SweepState::Running(AppMode::S11);
        state.change_mode(AppMode::Spec);
        assert!(!state.any_running());
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::StopSweep
        ));
    }

    #[test]
    fn parameter_builders_preserve_sample_counts_and_round_whole_hz() {
        let state = S11State {
            start_hz: 5000.6,
            ..S11State::default()
        };
        let params = state.s11_params().unwrap();
        assert_eq!(params.start_hz, 5001);
        assert_eq!(params.points, 201);
        assert_eq!(SpecState::default().spec_params().unwrap().points, 201);
    }

    #[test]
    fn mode_limits_and_non_finite_inputs_are_checked_before_casting() {
        for start_hz in [f64::NAN, f64::INFINITY, -1.0, 0.0, 4999.0] {
            let state = S11State {
                start_hz,
                ..S11State::default()
            };
            assert!(state.s11_params().is_err(), "{start_hz}");
        }
        for start_hz in [f64::NAN, f64::INFINITY, -0.1] {
            let state = SpecState {
                start_hz,
                ..SpecState::default()
            };
            assert!(state.spec_params().is_err(), "{start_hz}");
        }
        let spec = SpecState {
            start_hz: 0.0,
            stop_hz: 1000.0,
            ..SpecState::default()
        };
        assert_eq!(spec.spec_params().unwrap().start_hz, 0);
    }

    #[test]
    fn linked_fields_do_not_hide_reversed_or_negative_sweeps() {
        let mut state = S11State {
            start_hz: 1e6,
            stop_hz: 1e5,
            ..S11State::default()
        };
        state.start_stop_changed();
        assert_eq!((state.start_hz, state.stop_hz), (1e6, 1e5));
        assert!(state.s11_params().is_err());
        state.center_hz = 5000.0;
        state.span_hz = 20000.0;
        state.center_span_changed();
        assert_eq!(state.start_hz, -5000.0);
        assert!(state.s11_params().is_err());
        state.span_hz = -1000.0;
        state.center_span_changed();
        assert!(state.s11_params().is_err());
    }

    #[test]
    fn invalid_legacy_settings_are_preserved_but_cannot_run() {
        let cfg: crate::config::AppConfig = toml::from_str(
            "[s11]\nstart_hz = 0.0\npoints = 10001\ncal = 'calon'\n[spec]\nrbw = '100Hz'\n",
        )
        .unwrap();
        let mut state = AppState::default();
        cfg.apply_to(&mut state);
        assert_eq!(state.s11.start_hz, 0.0);
        assert_eq!(state.s11.points, 10001);
        assert_eq!(state.s11.cal, Cal::CalOn);
        assert!(state.s11.s11_params().is_err());
        assert!(state.spec.spec_params().is_err());
    }
}
