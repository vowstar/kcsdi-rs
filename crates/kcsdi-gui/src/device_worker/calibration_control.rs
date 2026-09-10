// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

use super::*;

type Owner = Option<(WorkerIdentity, CancellationToken)>;

pub(super) fn track_request(
    command: &CommandEnvelope,
    identity: WorkerIdentity,
    device: &Option<Device<TcpTransport>>,
    owner: &mut Owner,
) {
    if command.session_id != identity.session_id
        || command.request_id < identity.request_id
        || command.cancel.is_cancelled()
    {
        return;
    }
    let Some(dev) = device.as_ref() else { return };
    let report = dev.calibration_report();
    let accepted = match &command.command {
        WorkerCommand::StartCalibration(params) => {
            params.validate(&DEVICE_MODEL.capabilities()).is_ok()
                && !report.is_active()
                && !matches!(
                    dev.source_report().state,
                    SourceOutputState::Requested(_) | SourceOutputState::Unknown
                )
        }
        WorkerCommand::AdvanceCalibration(prompt) => {
            report.phase == CalibrationPhase::Prompt(*prompt)
        }
        WorkerCommand::CancelCalibration => report.is_active(),
        _ => false,
    };
    if accepted {
        *owner = Some((
            WorkerIdentity {
                session_id: command.session_id,
                request_id: command.request_id,
            },
            command.cancel.clone(),
        ));
    }
}

pub(super) fn rejection_guard(
    command: &CommandEnvelope,
    device: &Option<Device<TcpTransport>>,
    owner: &Owner,
) -> Option<CancellationToken> {
    let report = device.as_ref()?.calibration_report();
    if !report.is_active() {
        return None;
    }
    let may_proceed = match &command.command {
        WorkerCommand::Connect { .. }
        | WorkerCommand::Disconnect
        | WorkerCommand::Shutdown
        | WorkerCommand::CancelCalibration => true,
        WorkerCommand::AdvanceCalibration(prompt) => {
            report.phase == CalibrationPhase::Prompt(*prompt)
        }
        _ => false,
    };
    if may_proceed {
        None
    } else {
        owner.as_ref().map(|(_, cancel)| cancel.clone())
    }
}

pub(super) fn reject_while_active(
    command: &WorkerCommand,
    device: &Option<Device<TcpTransport>>,
    emit: &dyn Fn(WorkerEvent),
) -> bool {
    if !device
        .as_ref()
        .is_some_and(|dev| dev.calibration_report().is_active())
        || matches!(
            command,
            WorkerCommand::Connect { .. }
                | WorkerCommand::Disconnect
                | WorkerCommand::Shutdown
                | WorkerCommand::AdvanceCalibration(_)
                | WorkerCommand::CancelCalibration
        )
    {
        return false;
    }
    let message = "Finish or cancel calibration before another operation".into();
    if !matches!(command, WorkerCommand::RefreshStatus) {
        emit(WorkerEvent::CalibrationReport(
            device
                .as_ref()
                .expect("active calibration")
                .calibration_report(),
        ));
    }
    emit(if matches!(command, WorkerCommand::RefreshStatus) {
        WorkerEvent::StatusFailed(message)
    } else {
        WorkerEvent::Error(message)
    });
    true
}
