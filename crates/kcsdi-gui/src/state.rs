// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Application state and identities shared by the GUI and its device worker.

use std::sync::mpsc;
use std::time::Instant;

use kcsdi_core::calibration::{
    CalibrationParams, CalibrationPhase, CalibrationPrompt, CalibrationReport,
};
use kcsdi_core::commands::Format;
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::DeviceInfo;
use kcsdi_core::model::Model;
use kcsdi_core::source::{SourceParams, SourceReport};

use crate::acquisition::{SweepDelivery, SweepPlan};
use crate::health::{HealthSnapshot, HealthState};
use crate::i18n::{Language, StatusMessage, Text};
use crate::preview::PreviewMailbox;
use crate::source_panel::{InstrumentFunction, SourcePending, SourceUi};
use crate::workspace::Workspace;

pub const DEVICE_MODEL: Model = Model::Kc901V;

#[derive(Debug)]
pub enum WorkerCommand {
    Connect {
        target: kcsdi_core::connection::ConnectionTarget,
    },
    Disconnect,
    RunWorkspace(SweepPlan),
    StopSweep,
    StartSource(SourceParams),
    StopSource,
    StartCalibration(CalibrationParams),
    AdvanceCalibration(CalibrationPrompt),
    CancelCalibration,
    RefreshStatus,
    Shutdown,
}

#[derive(Debug)]
pub enum WorkerEvent {
    Connected(DeviceInfo),
    Disconnected,
    ConnectionLost(String),
    Error(String),
    SweepTrace(SweepDelivery),
    SweepStopped,
    RunProgress(crate::run_settings::RunProgress),
    SourceReport(SourceReport),
    CalibrationReport(CalibrationReport),
    Status(HealthSnapshot),
    StatusFailed(String),
}

#[derive(Debug)]
pub struct CommandEnvelope {
    pub session_id: u64,
    pub request_id: u64,
    pub cancel: CancellationToken,
    pub command: WorkerCommand,
}

#[derive(Debug)]
pub struct EventEnvelope {
    pub session_id: u64,
    pub request_id: u64,
    pub cycle_id: Option<u64>,
    pub event: WorkerEvent,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppMode {
    #[default]
    Spec,
    S11,
    S21,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SweepState {
    #[default]
    Idle,
    Running,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum S11Display {
    Phase,
    #[default]
    ReturnLoss,
    Vswr,
    Smith,
    Impedance,
    Magnitude,
    Resistance,
    Reactance,
}

impl S11Display {
    pub const ALL: [Self; 8] = [
        Self::Phase,
        Self::ReturnLoss,
        Self::Vswr,
        Self::Smith,
        Self::Impedance,
        Self::Magnitude,
        Self::Resistance,
        Self::Reactance,
    ];

    pub fn label(self, language: Language) -> &'static str {
        match self {
            Self::Magnitude => "|Z|",
            Self::Resistance => "R",
            Self::Reactance => "X",
            _ => language.text(match self {
                Self::Phase => Text::Phase,
                Self::ReturnLoss => Text::ReturnLoss,
                Self::Vswr => Text::Vswr,
                Self::Smith => Text::Smith,
                _ => Text::Impedance,
            }),
        }
    }

    pub fn wire_format(self) -> Format {
        match self {
            Self::Phase => Format::Ma,
            Self::ReturnLoss => Format::Loss,
            Self::Vswr => Format::Vswr,
            _ => Format::Z,
        }
    }

    pub fn default_y(self) -> (f64, f64) {
        match self {
            Self::Phase => (-180.0, 180.0),
            Self::ReturnLoss => (0.0, 50.0),
            Self::Vswr => (1.0, 10.0),
            Self::Smith => (-1.0, 1.0),
            Self::Reactance => (-100.0, 100.0),
            _ => (0.0, 200.0),
        }
    }

    pub fn y_label(self) -> &'static str {
        match self {
            Self::Phase => "deg",
            Self::ReturnLoss => "dB",
            Self::Vswr | Self::Smith => "",
            _ => "ohm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum S21Display {
    Phase,
    #[default]
    Loss,
    Delay,
}

impl S21Display {
    pub const ALL: [Self; 3] = [Self::Phase, Self::Loss, Self::Delay];

    pub fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Phase => Text::Phase,
            Self::Loss => Text::Loss,
            Self::Delay => Text::GroupDelay,
        })
    }

    pub fn wire_format(self) -> Format {
        match self {
            Self::Phase => Format::Ma,
            Self::Loss => Format::Loss,
            Self::Delay => Format::Delay,
        }
    }

    pub fn default_y(self) -> (f64, f64) {
        match self {
            Self::Phase => (-180.0, 180.0),
            Self::Loss => (-100.0, 20.0),
            Self::Delay => (-50e-9, 50e-9),
        }
    }

    pub fn y_label(self) -> &'static str {
        match self {
            Self::Phase => "deg",
            Self::Loss => "dB",
            Self::Delay => "s",
        }
    }
}

pub struct AppState {
    pub legacy_sweeps: Option<crate::config::LegacySweeps>,
    pub desktop: crate::desktop::DesktopState,
    pub language: Language,
    pub language_preference: crate::i18n::LanguagePreference,
    pub target: kcsdi_core::connection::ConnectionTarget,
    pub connection: ConnectionState,
    pub device_info: Option<DeviceInfo>,
    pub health: HealthState,
    pub workspace: Workspace,
    pub function: InstrumentFunction,
    pub source: SourceUi,
    pub calibration: crate::calibration_panel::CalibrationUi,
    pub active_plan: Option<SweepPlan>,
    pub run_progress: Option<crate::run_settings::RunProgress>,
    pub last_recording: Option<(u64, std::path::PathBuf)>,
    pub folder_opener: crate::folder_opener::FolderOpener,
    pub export: crate::export::ExportState,
    pub status_message: Option<StatusMessage>,
    pub session_id: u64,
    pub request_id: u64,
    pub sweep: SweepState,
    pub session_cancel: CancellationToken,
    pub acquisition_cancel: CancellationToken,
    pub worker_shutdown: CancellationToken,
    pub preview_mailbox: PreviewMailbox,
    pub cmd_tx: Option<mpsc::Sender<CommandEnvelope>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            legacy_sweeps: None,
            desktop: Default::default(),
            language: Default::default(),
            language_preference: Default::default(),
            target: Default::default(),
            connection: Default::default(),
            device_info: None,
            health: Default::default(),
            workspace: Default::default(),
            function: Default::default(),
            source: Default::default(),
            calibration: Default::default(),
            active_plan: None,
            run_progress: None,
            last_recording: None,
            folder_opener: crate::folder_opener::FolderOpener::default(),
            export: Default::default(),
            status_message: None,
            session_id: 0,
            request_id: 0,
            sweep: SweepState::Idle,
            session_cancel: Default::default(),
            acquisition_cancel: Default::default(),
            worker_shutdown: Default::default(),
            preview_mailbox: Default::default(),
            cmd_tx: None,
        }
    }
}

impl AppState {
    pub fn set_language_preference(&mut self, preference: crate::i18n::LanguagePreference) {
        self.language_preference = preference;
        self.language = preference.resolve();
    }

    /// Replace acquisition when its conditions, members or run settings change.
    pub fn reconcile_plan(&mut self) {
        if !self.any_running() || self.function != InstrumentFunction::Measurements {
            return;
        }
        match self.workspace.plan() {
            Ok(plan) if self.active_plan.as_ref() != Some(&plan) => {
                self.send(WorkerCommand::RunWorkspace(plan))
            }
            Err(error) => {
                self.status_message = Some(error.to_string().into());
                self.send(WorkerCommand::StopSweep);
            }
            _ => {}
        }
    }

    pub fn send(&mut self, command: WorkerCommand) {
        if self.calibration.busy()
            && matches!(
                command,
                WorkerCommand::RunWorkspace(_)
                    | WorkerCommand::StartSource(_)
                    | WorkerCommand::StopSweep
                    | WorkerCommand::StopSource
                    | WorkerCommand::StartCalibration(_)
            )
        {
            self.status_message = Some(StatusMessage::Text(Text::CalibrationBusy));
            return;
        }
        if matches!(command, WorkerCommand::StartCalibration(_))
            && (self.source.busy() || self.connection != ConnectionState::Connected)
        {
            self.status_message = Some(StatusMessage::Text(if self.source.busy() {
                Text::CalibrationStopSource
            } else {
                Text::DeviceInfoUnavailable
            }));
            return;
        }
        if let WorkerCommand::AdvanceCalibration(prompt) = &command
            && (self.calibration.pending.is_some()
                || self.calibration.report.phase != CalibrationPhase::Prompt(*prompt))
        {
            return;
        }
        if matches!(command, WorkerCommand::CancelCalibration)
            && (!self.calibration.busy()
                || self.calibration.pending == Some(crate::calibration_panel::Pending::Cancel))
        {
            return;
        }
        if matches!(command, WorkerCommand::RunWorkspace(_)) && self.source.busy() {
            self.status_message = Some(StatusMessage::Text(Text::SourceStopBeforeMeasure));
            return;
        }
        if matches!(command, WorkerCommand::RefreshStatus)
            && (self.connection != ConnectionState::Connected
                || self.source.busy()
                || self.calibration.busy()
                || !self.health.begin())
        {
            return;
        }
        if matches!(
            command,
            WorkerCommand::Connect { .. } | WorkerCommand::Disconnect | WorkerCommand::Shutdown
        ) {
            self.device_info = None;
            self.health = HealthState::default();
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
            self.run_progress = None;
            self.request_id = self
                .request_id
                .checked_add(1)
                .expect("request ID exhausted");
        }
        let cancel = match &command {
            WorkerCommand::StartCalibration(params) => {
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.active_plan = None;
                self.sweep = SweepState::Idle;
                self.calibration.begin(*params);
                let mode = crate::calibration_panel::affected_mode(params.kind());
                let stream = match mode {
                    AppMode::S11 => kcsdi_core::protocol::StreamMode::S11,
                    AppMode::S21 => kcsdi_core::protocol::StreamMode::S21,
                    AppMode::Spec => unreachable!(),
                };
                for trace in &mut self.workspace.traces {
                    if trace.settings.display.mode() == mode
                        || trace
                            .completed
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.data.mode == stream)
                    {
                        trace.analysis.invalidate_measurements();
                    }
                }
                self.acquisition_cancel.clone()
            }
            WorkerCommand::AdvanceCalibration(_) | WorkerCommand::CancelCalibration => {
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.calibration.pending =
                    Some(if matches!(command, WorkerCommand::CancelCalibration) {
                        crate::calibration_panel::Pending::Cancel
                    } else {
                        crate::calibration_panel::Pending::Advance
                    });
                self.acquisition_cancel.clone()
            }
            WorkerCommand::RunWorkspace(plan) => {
                self.last_recording = None;
                self.run_progress = Some(crate::run_settings::RunProgress::Acquiring);
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.active_plan = Some(plan.clone());
                self.sweep = SweepState::Running;
                self.acquisition_cancel.clone()
            }
            WorkerCommand::StartSource(params) => {
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.active_plan = None;
                self.sweep = SweepState::Idle;
                self.source.pending = Some(SourcePending::Start);
                self.source.requested = Some(*params);
                self.source.report.warning = None;
                self.acquisition_cancel.clone()
            }
            WorkerCommand::StopSource => {
                self.acquisition_cancel.cancel();
                self.active_plan = None;
                self.sweep = SweepState::Idle;
                self.source.pending = Some(SourcePending::Stop);
                CancellationToken::default()
            }
            WorkerCommand::StopSweep => {
                self.acquisition_cancel.cancel();
                self.active_plan = None;
                self.sweep = SweepState::Stopping;
                CancellationToken::default()
            }
            WorkerCommand::Connect { .. } => {
                self.source.lost();
                self.calibration.lost();
                self.active_plan = None;
                self.sweep = SweepState::Idle;
                self.connection = ConnectionState::Connecting;
                self.session_cancel.clone()
            }
            WorkerCommand::Disconnect | WorkerCommand::Shutdown => {
                self.source.lost();
                self.calibration.lost();
                self.active_plan = None;
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

    pub fn any_running(&self) -> bool {
        self.sweep == SweepState::Running
    }

    pub fn select_function(&mut self, function: InstrumentFunction) {
        if self.function == function {
            return;
        }
        if self.calibration.busy() {
            self.send(WorkerCommand::CancelCalibration);
            return;
        }
        if self.connection == ConnectionState::Connected {
            if self.source.busy() && self.source.pending != Some(SourcePending::Stop) {
                self.send(WorkerCommand::StopSource);
            } else if self.any_running() {
                self.send(WorkerCommand::StopSweep);
            }
        }
        self.function = function;
        self.workspace.editor = None;
        self.workspace.frequency_editor.cancel();
        self.workspace.run_editor.cancel();
    }

    pub fn stop_operation(&mut self) {
        if self.calibration.busy() {
            self.send(WorkerCommand::CancelCalibration);
        } else if self.source.busy() {
            if self.source.pending != Some(SourcePending::Stop) {
                self.send(WorkerCommand::StopSource);
            }
        } else {
            self.send(WorkerCommand::StopSweep);
        }
    }

    pub fn refresh_health_if_due(&mut self, now: Instant) {
        if self.connection == ConnectionState::Connected
            && !self.source.busy()
            && !self.calibration.busy()
            && self.sweep != SweepState::Stopping
            && self.health.due(now)
        {
            self.send(WorkerCommand::RefreshStatus);
        }
    }

    pub fn clear_preview(&mut self) {
        self.workspace.clear_previews();
        self.preview_mailbox.clear();
    }

    pub fn worker_stopped(&mut self) {
        self.source.lost();
        self.calibration.lost();
        self.session_cancel.cancel();
        self.acquisition_cancel.cancel();
        self.worker_shutdown.cancel();
        self.sweep = SweepState::Idle;
        self.active_plan = None;
        self.run_progress = None;
        self.clear_preview();
        self.device_info = None;
        self.health = HealthState::default();
        let message = "Device worker stopped".to_string();
        self.connection = ConnectionState::Error(message.clone());
        self.status_message = Some(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TraceDisplay;

    #[test]
    fn calibration_owns_controls_and_reconnect_never_resumes_a_step() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            connection: ConnectionState::Connected,
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.send(WorkerCommand::StartCalibration(
            CalibrationParams::S21System,
        ));
        let start = rx.try_recv().unwrap();
        state.calibration.accept(CalibrationReport {
            kind: Some(kcsdi_core::calibration::CalibrationKind::S21System),
            phase: CalibrationPhase::Prompt(CalibrationPrompt::Through),
        });
        let request_id = state.request_id;
        for command in [
            WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()),
            WorkerCommand::StartSource(
                state
                    .source
                    .config
                    .rf
                    .params(kcsdi_core::source::SourceKind::Rf)
                    .unwrap(),
            ),
            WorkerCommand::StopSweep,
            WorkerCommand::StopSource,
            WorkerCommand::RefreshStatus,
            WorkerCommand::StartCalibration(CalibrationParams::S11System),
            WorkerCommand::AdvanceCalibration(CalibrationPrompt::Short),
        ] {
            state.send(command);
        }
        assert_eq!(state.request_id, request_id);
        assert!(!state.health.pending);
        assert!(rx.try_recv().is_err());
        state.send(WorkerCommand::AdvanceCalibration(
            CalibrationPrompt::Through,
        ));
        let advance = rx.try_recv().unwrap();
        assert!(start.cancel.is_cancelled());
        assert!(!advance.cancel.is_cancelled());
        state.send(WorkerCommand::Disconnect);
        rx.try_recv().unwrap();
        assert!(advance.cancel.is_cancelled());
        assert_eq!(state.calibration.report.phase, CalibrationPhase::Unknown);
        state.send(WorkerCommand::Connect {
            target: kcsdi_core::connection::ConnectionTarget::Tcp {
                host: "127.0.0.1".into(),
                port: 901,
            },
        });
        let connect = rx.try_recv().unwrap();
        assert!(matches!(connect.command, WorkerCommand::Connect { .. }));
        assert_eq!(state.calibration.report.phase, CalibrationPhase::Unknown);
        assert!(!state.calibration.busy());
        state.connection = ConnectionState::Connected;
        state.send(WorkerCommand::AdvanceCalibration(
            CalibrationPrompt::Through,
        ));
        assert!(rx.try_recv().is_err());
        state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::RunWorkspace(_)
        ));
    }

    #[test]
    fn identities_and_cancellation_have_separate_lifetimes() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.send(WorkerCommand::Connect {
            target: kcsdi_core::connection::ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 901,
            },
        });
        let connect = rx.try_recv().unwrap();
        state.connection = ConnectionState::Connected;
        let plan = state.workspace.plan().unwrap();
        state.send(WorkerCommand::RunWorkspace(plan.clone()));
        let first = rx.try_recv().unwrap();
        state.send(WorkerCommand::RefreshStatus);
        let refresh = rx.try_recv().unwrap();
        assert_eq!((refresh.session_id, refresh.request_id), (1, 2));
        assert!(!first.cancel.is_cancelled());
        state.send(WorkerCommand::RunWorkspace(plan));
        let second = rx.try_recv().unwrap();
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled());
        assert!(!connect.cancel.is_cancelled());
        assert!(!refresh.cancel.is_cancelled());
        state.send(WorkerCommand::StopSweep);
        assert!(second.cancel.is_cancelled());
        assert_eq!(state.sweep, SweepState::Stopping);
        assert!(!state.worker_shutdown.is_cancelled());
        state.send(WorkerCommand::Disconnect);
        assert_eq!((state.session_id, state.request_id), (2, 5));
        assert!(connect.cancel.is_cancelled());
        assert!(refresh.cancel.is_cancelled());
        assert_eq!(state.connection, ConnectionState::Disconnecting);
        state.send(WorkerCommand::Shutdown);
        assert!(state.worker_shutdown.is_cancelled());
    }

    #[test]
    fn presentation_edits_preserve_request_but_membership_and_conditions_replace_it() {
        let mut state = AppState::default();
        state.workspace.selected_mut().unwrap().settings.display =
            TraceDisplay::S11(S11Display::Impedance);
        state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
        let request = state.request_id;
        state.workspace.selected_mut().unwrap().settings.display =
            TraceDisplay::S11(S11Display::Smith);
        state.workspace.selected_mut().unwrap().settings.line_width = 2.0;
        state.workspace.selected = None;
        state.reconcile_plan();
        assert_eq!(state.request_id, request);
        state.workspace.traces[0].settings.rbw = kcsdi_core::model::Rbw::R3k;
        state.reconcile_plan();
        assert_eq!(state.request_id, request + 1);
        state.workspace.traces[0].settings.visible = false;
        state.reconcile_plan();
        assert_eq!(state.sweep, SweepState::Stopping);
        assert!(state.active_plan.is_none());
    }

    #[test]
    fn a_closed_worker_cannot_leave_a_run_indicator() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
        assert_eq!(state.sweep, SweepState::Idle);
        assert!(matches!(state.connection, ConnectionState::Error(_)));
        assert!(state.worker_shutdown.is_cancelled());
    }

    #[test]
    fn manual_and_periodic_refresh_share_one_pending_request() {
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            connection: ConnectionState::Connected,
            session_id: 3,
            request_id: 7,
            cmd_tx: Some(tx),
            ..Default::default()
        };
        let now = Instant::now();
        state.refresh_health_if_due(now);
        for _ in 0..10 {
            state.send(WorkerCommand::RefreshStatus);
            state.refresh_health_if_due(now + std::time::Duration::from_secs(120));
        }
        let request = rx.try_recv().unwrap();
        assert!(matches!(request.command, WorkerCommand::RefreshStatus));
        assert_eq!((request.session_id, request.request_id), (3, 7));
        assert!(rx.try_recv().is_err());
        assert_eq!((state.session_id, state.request_id), (3, 7));
        state.health.fail("Temporary rejection".into(), now);
        state.refresh_health_if_due(now);
        assert!(rx.try_recv().is_err());
        state.refresh_health_if_due(now + crate::health::REFRESH_INTERVAL);
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::RefreshStatus
        ));
    }

    #[test]
    fn health_clock_only_queues_in_a_usable_session() {
        for connection in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Disconnecting,
            ConnectionState::Error("connection failed".into()),
        ] {
            let (tx, rx) = mpsc::channel();
            let mut state = AppState {
                connection,
                cmd_tx: Some(tx),
                ..Default::default()
            };
            state.refresh_health_if_due(Instant::now());
            state.send(WorkerCommand::RefreshStatus);
            assert!(!state.health.pending);
            assert!(rx.try_recv().is_err());
        }
        let (tx, rx) = mpsc::channel();
        let mut state = AppState {
            connection: ConnectionState::Connected,
            sweep: SweepState::Stopping,
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.refresh_health_if_due(Instant::now());
        assert!(rx.try_recv().is_err());
        state.sweep = SweepState::Idle;
        state.refresh_health_if_due(Instant::now());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn session_transitions_clear_identity_and_all_health_immediately() {
        for command in [
            WorkerCommand::Disconnect,
            WorkerCommand::Shutdown,
            WorkerCommand::Connect {
                target: kcsdi_core::connection::ConnectionTarget::Tcp {
                    host: "instrument.local".into(),
                    port: 901,
                },
            },
        ] {
            let mut state = AppState {
                connection: ConnectionState::Connected,
                device_info: Some(DeviceInfo {
                    serial: "previous".into(),
                    username: String::new(),
                    software: String::new(),
                    hardware: String::new(),
                    copyright: String::new(),
                }),
                ..Default::default()
            };
            state.health.succeed(
                crate::health::tests::snapshot(42.0, Instant::now()),
                Instant::now(),
            );
            state.health.error = Some("Old query failure".into());
            state.health.pending = true;
            state.send(command);
            assert!(state.device_info.is_none());
            assert!(state.health.snapshot.is_none());
            assert!(state.health.error.is_none());
            assert!(state.health.last_attempt.is_none());
            assert!(!state.health.pending);
        }
    }
}
