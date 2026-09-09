// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Application state and identities shared by the GUI and its device worker.

use std::sync::mpsc;

use kcsdi_core::commands::Format;
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::{DeviceInfo, Voltage};
use kcsdi_core::model::Model;

use crate::acquisition::{SweepDelivery, SweepPlan};
use crate::i18n::{Language, StatusMessage, Text};
use crate::preview::PreviewMailbox;
use crate::workspace::Workspace;

pub const DEVICE_MODEL: Model = Model::Kc901V;

#[derive(Debug)]
pub enum WorkerCommand {
    Connect { host: String, port: u16 },
    Disconnect,
    RunWorkspace(SweepPlan),
    StopSweep,
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
    Status { temperature: f64, voltage: Voltage },
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
    pub host: String,
    pub port: u16,
    pub connection: ConnectionState,
    pub device_info: Option<DeviceInfo>,
    pub temperature: Option<f64>,
    pub voltage: Option<Voltage>,
    pub workspace: Workspace,
    pub active_plan: Option<SweepPlan>,
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
            host: String::new(),
            port: 901,
            connection: Default::default(),
            device_info: None,
            temperature: None,
            voltage: None,
            workspace: Default::default(),
            active_plan: None,
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

    /// Replace the whole acquisition only when its wire settings or members change.
    pub fn reconcile_plan(&mut self) {
        if !self.any_running() {
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
            WorkerCommand::RunWorkspace(plan) => {
                self.acquisition_cancel.cancel();
                self.acquisition_cancel = CancellationToken::default();
                self.active_plan = Some(plan.clone());
                self.sweep = SweepState::Running;
                self.acquisition_cancel.clone()
            }
            WorkerCommand::StopSweep => {
                self.acquisition_cancel.cancel();
                self.active_plan = None;
                self.sweep = SweepState::Stopping;
                CancellationToken::default()
            }
            WorkerCommand::Connect { .. } => {
                self.active_plan = None;
                self.sweep = SweepState::Idle;
                self.connection = ConnectionState::Connecting;
                self.session_cancel.clone()
            }
            WorkerCommand::Disconnect | WorkerCommand::Shutdown => {
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

    pub fn clear_preview(&mut self) {
        self.workspace.clear_previews();
        self.preview_mailbox.clear();
    }

    pub fn worker_stopped(&mut self) {
        self.session_cancel.cancel();
        self.acquisition_cancel.cancel();
        self.worker_shutdown.cancel();
        self.sweep = SweepState::Idle;
        self.active_plan = None;
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
    use crate::workspace::TraceDisplay;

    #[test]
    fn identities_and_cancellation_have_separate_lifetimes() {
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
}
