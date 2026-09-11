// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Device worker thread: owns the instrument connection.
//!
//! Mirrors the ptouch-rs worker pattern. All blocking I/O happens here;
//! the UI sends [`WorkerCommand`]s and receives [`WorkerEvent`]s over
//! two mpsc channels. After every event the worker calls
//! `ctx.request_repaint()` so the UI picks it up immediately.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use kcsdi_core::Device;
use kcsdi_core::calibration::{CalibrationPhase, CalibrationReport};
#[cfg(test)]
use kcsdi_core::connection::ConnectionTarget;
use kcsdi_core::connection::ConnectionTransport;
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::SweepData;
use kcsdi_core::source::{SourceOutputState, SourceReport};
#[cfg(test)]
use kcsdi_core::transport::TcpTransport;
use log::{error, info};

use crate::acquisition::{AcquisitionSettings, CompletedSweep, SweepDelivery, SweepPlan, TraceId};
use crate::preview::{PreviewEnvelope, PreviewMailbox};
use crate::recording::{RecordKey, RecordResult, RecordWriter};
use crate::run_settings::RunProgress;
use crate::spreadsheet::FrozenSnapshots;
use crate::state::{CommandEnvelope, DEVICE_MODEL, EventEnvelope, WorkerCommand, WorkerEvent};

pub const EVENT_CAPACITY: usize = 16;
const STATUS_COMMAND_GAP: Duration = Duration::from_millis(100);
const WORKER_POLL: Duration = Duration::from_millis(50);

#[cfg(test)]
mod source_tests;

mod calibration_control;

#[cfg(test)]
mod calibration_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunPhase {
    Acquiring,
    Saving,
    Waiting { until: Instant },
}

#[derive(Debug, Clone)]
struct SweepRequest {
    request_id: u64,
    cancel: CancellationToken,
    plan: SweepPlan,
    next_group: usize,
    pass_id: u64,
    pass: Vec<(TraceId, Arc<CompletedSweep>)>,
    phase: RunPhase,
}

impl SweepRequest {
    fn new(request_id: u64, cancel: CancellationToken, plan: SweepPlan) -> Self {
        Self {
            request_id,
            cancel,
            plan,
            next_group: 0,
            pass_id: 1,
            pass: Vec::new(),
            phase: RunPhase::Acquiring,
        }
    }

    fn finish_pass(&mut self, now: Instant) -> Result<RunProgress, String> {
        self.pass_id = self
            .pass_id
            .checked_add(1)
            .ok_or("recording pass ID exhausted")?;
        let interval = self.plan.run.interval();
        if interval.is_zero() {
            self.phase = RunPhase::Acquiring;
            Ok(RunProgress::Acquiring)
        } else {
            let until = now
                .checked_add(interval)
                .ok_or("run interval exceeds the host clock range")?;
            self.phase = RunPhase::Waiting { until };
            Ok(RunProgress::Waiting { until })
        }
    }
}

#[derive(Default)]
struct RecordingState {
    writer: Option<RecordWriter>,
    pending: Option<RecordKey>,
}

impl RecordingState {
    fn finish_pass(
        &mut self,
        session_id: u64,
        job: &mut SweepRequest,
    ) -> Result<RunProgress, String> {
        if !job.plan.run.recording.enabled {
            return job.finish_pass(Instant::now());
        }
        if self.pending.is_some() {
            return Err("a previous recording task is still pending".into());
        }
        let snapshots = FrozenSnapshots::new(std::mem::take(&mut job.pass))?;
        let key = RecordKey {
            session_id,
            request_id: job.request_id,
            pass_id: job.pass_id,
        };
        if self.writer.is_none() {
            self.writer = Some(RecordWriter::new()?);
        }
        self.writer.as_mut().expect("created above").try_save(
            key,
            job.plan.run.recording.clone(),
            snapshots,
            job.cancel.clone(),
        )?;
        self.pending = Some(key);
        job.phase = RunPhase::Saving;
        Ok(RunProgress::Saving)
    }

    fn poll(
        &mut self,
        identity: WorkerIdentity,
        job: &mut Option<SweepRequest>,
        emit: &dyn Fn(EventEnvelope),
    ) -> bool {
        if let Some(result) = self.writer.as_mut().and_then(RecordWriter::poll) {
            self.accept_result(result, identity, job, Instant::now(), emit);
            true
        } else {
            false
        }
    }

    fn accept_result(
        &mut self,
        result: RecordResult,
        identity: WorkerIdentity,
        job: &mut Option<SweepRequest>,
        now: Instant,
        emit: &dyn Fn(EventEnvelope),
    ) {
        if self.pending != Some(result.key) {
            return;
        }
        self.pending = None;
        let Some(current) = job.as_mut() else { return };
        if current.cancel.is_cancelled()
            || identity.session_id != result.key.session_id
            || current.request_id != result.key.request_id
            || current.pass_id != result.key.pass_id
            || current.phase != RunPhase::Saving
        {
            return;
        }
        let source = WorkerIdentity {
            session_id: result.key.session_id,
            request_id: result.key.request_id,
        };
        let result = match result.result {
            Ok(Some(path)) => {
                emit(source.event(WorkerEvent::RunProgress(RunProgress::Saved {
                    path,
                    pass_id: result.key.pass_id,
                })));
                current.finish_pass(now)
            }
            Ok(None) => {
                discard_job(job);
                return;
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(progress) => emit(source.event(WorkerEvent::RunProgress(progress))),
            Err(error) => recording_failed(error, job, &|event| emit(source.event(event))),
        }
    }
}

fn discard_job(job: &mut Option<SweepRequest>) {
    if let Some(job) = job.take() {
        job.cancel.cancel();
    }
}

fn recording_failed(error: String, job: &mut Option<SweepRequest>, emit: &dyn Fn(WorkerEvent)) {
    discard_job(job);
    emit(WorkerEvent::Error(format!("Recording failed: {error}")));
}

fn advance_wait(job: &mut SweepRequest, writer_pending: bool, now: Instant) -> Option<RunProgress> {
    if writer_pending {
        return None;
    }
    match job.phase {
        RunPhase::Saving => {}
        RunPhase::Waiting { until } if now >= until => {}
        _ => return None,
    }
    job.phase = RunPhase::Acquiring;
    Some(RunProgress::Acquiring)
}

#[derive(Clone, Copy, Default)]
struct WorkerIdentity {
    session_id: u64,
    request_id: u64,
}

impl WorkerIdentity {
    fn event(self, event: WorkerEvent) -> EventEnvelope {
        EventEnvelope {
            session_id: self.session_id,
            request_id: self.request_id,
            cycle_id: None,
            event,
        }
    }
}

/// Entry point of the `"device-worker"` thread.
pub fn device_worker(
    cmd_rx: mpsc::Receiver<CommandEnvelope>,
    evt_tx: mpsc::SyncSender<EventEnvelope>,
    ctx: egui::Context,
    shutdown: CancellationToken,
    preview: PreviewMailbox,
) {
    run_worker(
        cmd_rx,
        evt_tx,
        ctx,
        shutdown,
        preview,
        RecordingState::default(),
    );
}

fn run_worker(
    cmd_rx: mpsc::Receiver<CommandEnvelope>,
    evt_tx: mpsc::SyncSender<EventEnvelope>,
    ctx: egui::Context,
    shutdown: CancellationToken,
    preview: PreviewMailbox,
    mut recording: RecordingState,
) {
    let mut device: Option<Device<ConnectionTransport>> = None;
    let mut job: Option<SweepRequest> = None;
    let mut identity = WorkerIdentity::default();
    let mut cycle_id = 0_u64;
    let mut pending_status = None;
    let mut source_request = None;
    let mut calibration_request = None;

    let emit = |evt: EventEnvelope| {
        send_event(&evt_tx, evt, &shutdown, None, &ctx);
    };

    'worker: loop {
        if shutdown.is_cancelled() {
            break;
        }
        if device.is_none()
            || job
                .as_ref()
                .is_none_or(|job| job.phase != RunPhase::Acquiring)
            || recording.pending.is_some()
        {
            match cmd_rx.recv_timeout(WORKER_POLL) {
                Ok(cmd) => {
                    if shutdown.is_cancelled() || matches!(cmd.command, WorkerCommand::Shutdown) {
                        break;
                    }
                    let cancel = cmd.cancel.clone();
                    track_source_request(&cmd, identity, &mut source_request);
                    calibration_control::track_request(
                        &cmd,
                        identity,
                        &device,
                        &mut calibration_request,
                    );
                    let guard =
                        source_rejection_guard(&cmd, &device, &source_request).or_else(|| {
                            calibration_control::rejection_guard(
                                &cmd,
                                &device,
                                &calibration_request,
                            )
                        });
                    handle_or_defer(
                        cmd,
                        &mut pending_status,
                        &mut identity,
                        &mut device,
                        &mut job,
                        recording.pending.is_some(),
                        &|event| {
                            send_request_event(
                                &evt_tx,
                                event,
                                &shutdown,
                                &cancel,
                                guard.as_ref(),
                                &ctx,
                            )
                        },
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Control commands take precedence over health queries and the next group.
        loop {
            if shutdown.is_cancelled() {
                break 'worker;
            }
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if matches!(cmd.command, WorkerCommand::Shutdown) {
                        break 'worker;
                    }
                    let cancel = cmd.cancel.clone();
                    track_source_request(&cmd, identity, &mut source_request);
                    calibration_control::track_request(
                        &cmd,
                        identity,
                        &device,
                        &mut calibration_request,
                    );
                    let guard =
                        source_rejection_guard(&cmd, &device, &source_request).or_else(|| {
                            calibration_control::rejection_guard(
                                &cmd,
                                &device,
                                &calibration_request,
                            )
                        });
                    handle_or_defer(
                        cmd,
                        &mut pending_status,
                        &mut identity,
                        &mut device,
                        &mut job,
                        recording.pending.is_some(),
                        &|event| {
                            send_request_event(
                                &evt_tx,
                                event,
                                &shutdown,
                                &cancel,
                                guard.as_ref(),
                                &ctx,
                            )
                        },
                    );
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'worker,
            }
        }
        if let Some(status) = pending_status.take() {
            let cancel = status.cancel.clone();
            let guard = source_rejection_guard(&status, &device, &source_request).or_else(|| {
                calibration_control::rejection_guard(&status, &device, &calibration_request)
            });
            handle(status, &mut identity, &mut device, &mut job, &|event| {
                send_request_event(&evt_tx, event, &shutdown, &cancel, guard.as_ref(), &ctx);
            });
        }
        if shutdown.is_cancelled() {
            break;
        }
        if let Some(dev) = device.as_mut()
            && dev.calibration_report().is_active()
        {
            let (owner, cancel) = calibration_request
                .as_ref()
                .expect("calibration start has an owner");
            let before = dev.calibration_report();
            // Next replaces the request token at a human prompt. Polling that
            // prompt must not treat the replacement itself as an abort.
            let poll_cancel = if matches!(before.phase, CalibrationPhase::Prompt(_)) {
                &shutdown
            } else {
                cancel
            };
            match dev.poll_calibration_controlled(poll_cancel) {
                Ok(report) if report != before => {
                    send_event(
                        &evt_tx,
                        owner.event(WorkerEvent::CalibrationReport(report)),
                        &shutdown,
                        Some(cancel),
                        &ctx,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    let report = dev.calibration_report();
                    // Release the procedure before waiting for UI capacity.
                    if let Some(mut dev) = device.take() {
                        dev.close();
                    }
                    discard_job(&mut job);
                    send_event(
                        &evt_tx,
                        owner.event(WorkerEvent::CalibrationReport(report)),
                        &shutdown,
                        Some(cancel),
                        &ctx,
                    );
                    emit(owner.event(WorkerEvent::ConnectionLost(format!(
                        "Calibration failed: {error}"
                    ))));
                }
            }
            continue;
        }
        if let Some(dev) = device.as_mut()
            && matches!(dev.source_report().state, SourceOutputState::Requested(_))
        {
            let (source, cancel) = source_request.as_ref().expect("source start has an owner");
            let before = dev.source_report();
            match dev.poll_source_controlled(&shutdown) {
                Ok(report) if report != before => {
                    send_event(
                        &evt_tx,
                        source.event(WorkerEvent::SourceReport(report)),
                        &shutdown,
                        Some(cancel),
                        &ctx,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    // A late failure can leave output enabled. Attempt stop
                    // and local release before waiting for UI event capacity.
                    let report = dev.source_report();
                    if let Some(mut dev) = device.take() {
                        dev.close();
                    }
                    discard_job(&mut job);
                    send_event(
                        &evt_tx,
                        source.event(WorkerEvent::SourceReport(report)),
                        &shutdown,
                        Some(cancel),
                        &ctx,
                    );
                    emit(source.event(WorkerEvent::ConnectionLost(format!(
                        "Source failed: {error}"
                    ))));
                }
            }
            // Polling may have received a control command or filled the event
            // queue. Drain control before considering another acquisition.
            continue;
        }
        let cancel = job
            .as_ref()
            .map(|job| job.cancel.clone())
            .unwrap_or_default();
        if recording.poll(identity, &mut job, &|event| {
            send_request_event(&evt_tx, event, &shutdown, &cancel, None, &ctx);
        }) {
            // A progress event may have waited for UI capacity. Recheck queued
            // control and health commands before starting another acquisition.
            continue;
        }
        if job.as_ref().is_some_and(|job| job.cancel.is_cancelled()) {
            discard_job(&mut job);
        }
        if let Some(job) = job.as_mut()
            && let Some(progress) = advance_wait(job, recording.pending.is_some(), Instant::now())
        {
            let source = WorkerIdentity {
                session_id: identity.session_id,
                request_id: job.request_id,
            };
            send_event(
                &evt_tx,
                source.event(WorkerEvent::RunProgress(progress)),
                &shutdown,
                Some(&job.cancel),
                &ctx,
            );
        }
        if recording.pending.is_some()
            || job
                .as_ref()
                .is_some_and(|job| job.phase != RunPhase::Acquiring)
        {
            continue;
        }
        if shutdown.is_cancelled() {
            break;
        }
        if device.is_some() && job.is_some() {
            // A stream is owned exclusively until its complete frame or cleanup.
            let current = job.clone().expect("checked above");
            if current.cancel.is_cancelled() {
                discard_job(&mut job);
                continue;
            }
            let group = &current.plan.groups[current.next_group];
            cycle_id = cycle_id.checked_add(1).expect("sweep cycle ID exhausted");
            let source = WorkerIdentity {
                session_id: identity.session_id,
                request_id: current.request_id,
            };
            let emit_result = |event| emit(source.event(event));
            let mut last_preview = None;
            let mut progress =
                |progress: kcsdi_core::device::SweepProgress<'_>,
                 segment: Option<crate::preview::SegmentProgress>| {
                    if current.cancel.is_cancelled()
                        || shutdown.is_cancelled()
                        || !preview_due(
                            last_preview,
                            progress.points.is_empty()
                                || segment.is_some_and(|segment| segment.segment_points == 0),
                        )
                    {
                        return;
                    }
                    last_preview = Some(Instant::now());
                    preview.publish(PreviewEnvelope {
                        segment,
                        session_id: source.session_id,
                        request_id: source.request_id,
                        cycle_id,
                        data: SweepData {
                            mode: progress.mode,
                            format: progress.format.to_owned(),
                            points: progress.points.to_vec(),
                        },
                        group: group.clone(),
                    });
                    ctx.request_repaint();
                };
            let dev = device.as_mut().expect("checked above");
            let mut segment_metadata = None;
            let result = match &group.settings {
                AcquisitionSettings::Spec(params) => {
                    dev.sweep_spec_controlled(params, &current.cancel, |p| progress(p, None))
                }
                AcquisitionSettings::S11(params) => {
                    dev.sweep_s11_controlled(params, &current.cancel, |p| progress(p, None))
                }
                AcquisitionSettings::S21(params) => {
                    dev.sweep_s21_controlled(params, &current.cancel, |p| progress(p, None))
                }
                AcquisitionSettings::Segments(plan) => {
                    crate::segmented::acquire(dev, plan, &current.cancel, |p, segment| {
                        progress(p, Some(segment))
                    })
                    .map(|(data, metadata)| {
                        segment_metadata = Some(metadata);
                        data
                    })
                }
                AcquisitionSettings::List {
                    settings,
                    frequencies_hz,
                } => measure_list(dev, settings, frequencies_hz, &current.cancel, |p| {
                    progress(p, None)
                }),
            };
            match result {
                Ok(data) => {
                    let snapshot = Arc::new(CompletedSweep {
                        segments: segment_metadata,
                        data,
                        settings: group.settings.clone(),
                        session_id: source.session_id,
                        completed_at: SystemTime::now(),
                    });
                    let mut event = source.event(WorkerEvent::SweepTrace(SweepDelivery {
                        members: group.members.clone(),
                        snapshot: snapshot.clone(),
                    }));
                    event.cycle_id = Some(cycle_id);
                    send_event(&evt_tx, event, &shutdown, Some(&current.cancel), &ctx);
                    if current.cancel.is_cancelled() || shutdown.is_cancelled() {
                        discard_job(&mut job);
                        continue;
                    }
                    if let Some(active) = job.as_mut() {
                        if active.plan.run.recording.enabled {
                            active
                                .pass
                                .extend(group.members.iter().map(|id| (*id, snapshot.clone())));
                        }
                        active.next_group = (current.next_group + 1) % current.plan.groups.len();
                        if active.next_group == 0 {
                            match recording.finish_pass(source.session_id, active) {
                                Ok(progress) => {
                                    if active.phase != current.phase {
                                        send_event(
                                            &evt_tx,
                                            source.event(WorkerEvent::RunProgress(progress)),
                                            &shutdown,
                                            Some(&current.cancel),
                                            &ctx,
                                        );
                                    }
                                }
                                Err(error) => recording_failed(error, &mut job, &emit_result),
                            }
                        }
                    }
                }
                Err(e) => {
                    if !matches!(e, kcsdi_core::Error::Cancelled) {
                        error!("sweep failed: {e}");
                    }
                    fail(e, "Sweep failed", &mut device, &mut job, &emit_result);
                }
            }
        }
    }
    shutdown.cancel();
    discard_job(&mut job);
    if let Some(mut dev) = device.take() {
        dev.close();
    }
    // A filesystem call may be uninterruptible. Release remote control before
    // the writer joins, regardless of how long that final file operation takes.
    drop(recording);
}

fn track_source_request(
    command: &CommandEnvelope,
    identity: WorkerIdentity,
    source_request: &mut Option<(WorkerIdentity, CancellationToken)>,
) {
    if matches!(&command.command, WorkerCommand::StartSource(params)
        if params.validate(&DEVICE_MODEL.capabilities()).is_ok())
        && command.session_id == identity.session_id
        && command.request_id >= identity.request_id
        && !command.cancel.is_cancelled()
    {
        *source_request = Some((
            WorkerIdentity {
                session_id: command.session_id,
                request_id: command.request_id,
            },
            command.cancel.clone(),
        ));
    }
}

fn source_rejection_guard(
    command: &CommandEnvelope,
    device: &Option<Device<ConnectionTransport>>,
    owner: &Option<(WorkerIdentity, CancellationToken)>,
) -> Option<CancellationToken> {
    let rejects_without_stopping = match &command.command {
        WorkerCommand::RefreshStatus
        | WorkerCommand::RunWorkspace(_)
        | WorkerCommand::StartCalibration(_)
        | WorkerCommand::AdvanceCalibration(_)
        | WorkerCommand::CancelCalibration => true,
        WorkerCommand::StartSource(params) => {
            params.validate(&DEVICE_MODEL.capabilities()).is_err()
        }
        _ => false,
    };
    if rejects_without_stopping
        && device
            .as_ref()
            .is_some_and(|dev| matches!(dev.source_report().state, SourceOutputState::Requested(_)))
    {
        owner.as_ref().map(|(_, cancel)| cancel.clone())
    } else {
        None
    }
}

fn measure_list<T: kcsdi_core::transport::Transport>(
    device: &mut Device<T>,
    settings: &kcsdi_core::device::PointSettings,
    frequencies_hz: &[u64],
    cancel: &CancellationToken,
    mut progress: impl FnMut(kcsdi_core::device::SweepProgress<'_>),
) -> kcsdi_core::Result<SweepData> {
    let mut data = SweepData {
        mode: settings.mode(),
        format: settings.format().to_owned(),
        points: Vec::with_capacity(frequencies_hz.len()),
    };
    progress(kcsdi_core::device::SweepProgress {
        mode: data.mode,
        format: &data.format,
        points: &data.points,
        expected_points: frequencies_hz.len() as u32,
    });
    for &frequency_hz in frequencies_hz {
        if cancel.is_cancelled() {
            return Err(kcsdi_core::Error::Cancelled);
        }
        let point = device.measure_point_controlled(
            &kcsdi_core::device::PointParams {
                settings: settings.clone(),
                frequency_hz,
            },
            cancel,
        )?;
        data.points.extend(point.points);
        progress(kcsdi_core::device::SweepProgress {
            mode: data.mode,
            format: &data.format,
            points: &data.points,
            expected_points: frequencies_hz.len() as u32,
        });
    }
    if cancel.is_cancelled() {
        return Err(kcsdi_core::Error::Cancelled);
    }
    Ok(data)
}

fn handle_or_defer(
    envelope: CommandEnvelope,
    pending_status: &mut Option<CommandEnvelope>,
    identity: &mut WorkerIdentity,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    writer_pending: bool,
    emit: &dyn Fn(EventEnvelope),
) {
    if matches!(envelope.command, WorkerCommand::RefreshStatus) {
        if envelope.session_id == identity.session_id {
            *pending_status = Some(envelope);
        }
    } else {
        handle_with_recording(envelope, identity, device, job, writer_pending, emit);
        if pending_status
            .as_ref()
            .is_some_and(|status| status.session_id != identity.session_id)
        {
            *pending_status = None;
        }
    }
}

fn preview_due(last: Option<Instant>, reset: bool) -> bool {
    reset || last.is_none_or(|last| last.elapsed() >= Duration::from_millis(50))
}

/// Completed results are reliable while active, without blocking shutdown.
fn send_event(
    sender: &mpsc::SyncSender<EventEnvelope>,
    mut event: EventEnvelope,
    shutdown: &CancellationToken,
    obsolete: Option<&CancellationToken>,
    ctx: &egui::Context,
) {
    while !shutdown.is_cancelled() && !obsolete.is_some_and(CancellationToken::is_cancelled) {
        match sender.try_send(event) {
            Ok(()) => {
                ctx.request_repaint();
                return;
            }
            Err(mpsc::TrySendError::Full(pending)) => {
                event = pending;
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                shutdown.cancel();
                return;
            }
        }
    }
}

fn send_request_event(
    sender: &mpsc::SyncSender<EventEnvelope>,
    event: EventEnvelope,
    shutdown: &CancellationToken,
    cancel: &CancellationToken,
    source_rejection: Option<&CancellationToken>,
    ctx: &egui::Context,
) {
    let obsolete = match &event.event {
        WorkerEvent::RunProgress(_) | WorkerEvent::SourceReport(_) => Some(cancel),
        WorkerEvent::CalibrationReport(_) => source_rejection.or(Some(cancel)),
        WorkerEvent::Status(_) | WorkerEvent::StatusFailed(_) => source_rejection.or(Some(cancel)),
        WorkerEvent::Error(_) => source_rejection,
        _ => None,
    };
    // Progress becomes obsolete on Stop. A terminal error remains deliverable
    // after discarding the failed job has cancelled its acquisition token.
    send_event(sender, event, shutdown, obsolete, ctx);
}

/// Apply one command to the worker-local connection state.
fn handle(
    envelope: CommandEnvelope,
    identity: &mut WorkerIdentity,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    emit_event: &dyn Fn(EventEnvelope),
) {
    handle_with_recording(envelope, identity, device, job, false, emit_event);
}

fn handle_with_recording(
    envelope: CommandEnvelope,
    identity: &mut WorkerIdentity,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    writer_pending: bool,
    emit_event: &dyn Fn(EventEnvelope),
) {
    let CommandEnvelope {
        session_id,
        request_id,
        cancel,
        command,
    } = envelope;
    if matches!(
        command,
        WorkerCommand::Connect { .. } | WorkerCommand::Disconnect | WorkerCommand::Shutdown
    ) {
        if session_id <= identity.session_id {
            return;
        }
        *identity = WorkerIdentity {
            session_id,
            request_id,
        };
    } else {
        if session_id != identity.session_id
            || (!matches!(command, WorkerCommand::RefreshStatus)
                && request_id < identity.request_id)
        {
            return;
        }
        if !matches!(command, WorkerCommand::RefreshStatus) {
            identity.request_id = request_id;
        }
    }
    let source = WorkerIdentity {
        session_id,
        request_id,
    };
    let emit = |event| emit_event(source.event(event));
    if calibration_control::reject_while_active(&command, device, &emit) {
        return;
    }
    match command {
        WorkerCommand::Connect { target } => {
            discard_job(job);
            // Drop an old session before opening the device's single
            // control connection, including reconnect after a failure.
            *device = None;
            match target
                .connect_controlled(DEVICE_MODEL, &cancel)
                .and_then(|mut dev| {
                    let info = dev.device_info_controlled(&cancel)?;
                    Ok((dev, info))
                }) {
                Ok((dev, info)) => {
                    info!(
                        "connected to {}, serial {}",
                        target.to_string().escape_default(),
                        info.serial.escape_default()
                    );
                    *device = Some(dev);
                    emit(WorkerEvent::Connected(info));
                }
                Err(e) => {
                    *device = None;
                    if !matches!(e, kcsdi_core::Error::Cancelled) {
                        error!(
                            "connect to {} failed: {e}",
                            target.to_string().escape_default()
                        );
                        emit(WorkerEvent::Error(format!("Connect failed: {e}")));
                    }
                }
            }
        }
        WorkerCommand::Disconnect | WorkerCommand::Shutdown => {
            discard_job(job);
            if let Some(mut dev) = device.take() {
                dev.close();
            }
            emit(WorkerEvent::Disconnected);
        }
        WorkerCommand::RunWorkspace(plan) => {
            start_sweep(plan, request_id, cancel, device, job, &|event| {
                if writer_pending
                    && matches!(event, WorkerEvent::RunProgress(RunProgress::Acquiring))
                {
                    emit(WorkerEvent::RunProgress(RunProgress::Saving));
                } else {
                    emit(event);
                }
            });
            if writer_pending && let Some(job) = job.as_mut() {
                job.phase = RunPhase::Saving;
            }
        }
        WorkerCommand::StopSweep => {
            discard_job(job);
            if let Some(dev) = device.as_mut()
                && let Err(e) = dev.stop_sweep()
            {
                fail(e, "Stop failed", device, job, &emit);
            } else {
                emit(WorkerEvent::SweepStopped);
            }
        }
        WorkerCommand::StartSource(params) => {
            discard_job(job);
            if cancel.is_cancelled() {
                return;
            }
            let result = device
                .as_mut()
                .ok_or(kcsdi_core::Error::NotConnected)
                .and_then(|dev| {
                    params.validate(&DEVICE_MODEL.capabilities())?;
                    dev.stop_sweep()?;
                    dev.start_source_controlled(&params, &cancel)
                });
            source_result(result, device, job, &emit);
        }
        WorkerCommand::StopSource => {
            discard_job(job);
            let result = device
                .as_mut()
                .ok_or(kcsdi_core::Error::NotConnected)
                .and_then(|dev| dev.stop_source_controlled(&cancel));
            source_result(result, device, job, &emit);
        }
        WorkerCommand::StartCalibration(params) => {
            if cancel.is_cancelled() {
                return;
            }
            let result = device
                .as_mut()
                .ok_or(kcsdi_core::Error::NotConnected)
                .and_then(|dev| {
                    params.validate(&DEVICE_MODEL.capabilities())?;
                    discard_job(job);
                    dev.start_calibration_controlled(&params, &cancel)
                });
            calibration_result(result, device, job, &emit);
        }
        WorkerCommand::AdvanceCalibration(prompt) => {
            let result = device
                .as_mut()
                .ok_or(kcsdi_core::Error::NotConnected)
                .and_then(|dev| dev.advance_calibration_controlled(prompt, &cancel));
            calibration_result(result, device, job, &emit);
        }
        WorkerCommand::CancelCalibration => {
            let result = device
                .as_mut()
                .ok_or(kcsdi_core::Error::NotConnected)
                .and_then(|dev| dev.cancel_calibration_controlled(&cancel));
            calibration_result(result, device, job, &emit);
        }
        WorkerCommand::RefreshStatus => {
            refresh_status(&cancel, device, job, &emit);
        }
    }
}

fn calibration_result(
    result: kcsdi_core::Result<CalibrationReport>,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    match result {
        Ok(report) => emit(WorkerEvent::CalibrationReport(report)),
        Err(error) => {
            let report = device.as_ref().map_or(
                CalibrationReport {
                    kind: None,
                    phase: CalibrationPhase::Unknown,
                },
                Device::calibration_report,
            );
            // Core cleanup normally retires a failed calibration. Close here
            // too before a terminal report can wait behind a full UI queue.
            let retired = device.as_ref().is_some_and(Device::requires_reconnect);
            if retired && let Some(mut dev) = device.take() {
                dev.close();
            }
            emit(WorkerEvent::CalibrationReport(report));
            if retired {
                discard_job(job);
                emit(WorkerEvent::ConnectionLost(format!(
                    "Calibration failed: {error}"
                )));
            } else {
                fail(error, "Calibration failed", device, job, emit);
            }
        }
    }
}

fn source_result(
    result: kcsdi_core::Result<SourceReport>,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    match result {
        Ok(report) => emit(WorkerEvent::SourceReport(report)),
        Err(error) => {
            emit(WorkerEvent::SourceReport(device.as_ref().map_or(
                SourceReport {
                    state: SourceOutputState::Unknown,
                    warning: None,
                },
                Device::source_report,
            )));
            fail(error, "Source failed", device, job, emit);
        }
    }
}

fn refresh_status(
    cancel: &CancellationToken,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    let Some(dev) = device.as_mut() else { return };
    if matches!(
        dev.source_report().state,
        SourceOutputState::Requested(_) | SourceOutputState::Unknown
    ) {
        emit(WorkerEvent::StatusFailed(
            "Stop the source before refreshing status".into(),
        ));
        return;
    }
    // Status belongs to the session, not a sweep. Stop cannot interrupt an
    // in-flight pair, which retains two 10-second query budgets and this gap.
    // Disconnect and shutdown cancel the session token instead.
    let observed_at = Instant::now();
    let result = dev.temperature_controlled(cancel).and_then(|temperature| {
        status_command_gap(cancel)?;
        dev.voltage_controlled(cancel)
            .map(|voltage| crate::health::HealthSnapshot {
                temperature,
                voltage,
                observed_at,
            })
    });
    match result {
        Ok(snapshot) => emit(WorkerEvent::Status(snapshot)),
        Err(error)
            if connection_failed(&error)
                || device.as_ref().is_some_and(Device::requires_reconnect) =>
        {
            fail(error, "Status query failed", device, job, emit);
        }
        Err(kcsdi_core::Error::Cancelled) => {}
        Err(error) => emit(WorkerEvent::StatusFailed(format!(
            "Status query failed: {error}"
        ))),
    }
}

fn status_command_gap(cancel: &CancellationToken) -> kcsdi_core::Result<()> {
    let started = Instant::now();
    loop {
        if cancel.is_cancelled() {
            return Err(kcsdi_core::Error::Cancelled);
        }
        let Some(remaining) = STATUS_COMMAND_GAP.checked_sub(started.elapsed()) else {
            return Ok(());
        };
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn start_sweep(
    next: SweepPlan,
    request_id: u64,
    cancel: CancellationToken,
    device: &mut Option<Device<ConnectionTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    discard_job(job);
    if cancel.is_cancelled() {
        return;
    }
    if let Err(error) = next.validate() {
        fail(error, "Run failed", device, job, emit);
    } else if device.is_none() {
        fail(
            kcsdi_core::Error::NotConnected,
            "Run failed",
            device,
            job,
            emit,
        );
    } else if device.as_ref().is_some_and(|dev| {
        matches!(
            dev.source_report().state,
            SourceOutputState::Requested(_) | SourceOutputState::Unknown
        )
    }) {
        fail(
            kcsdi_core::Error::DeviceBusy("stop the source before starting a sweep".into()),
            "Run failed",
            device,
            job,
            emit,
        );
    } else {
        *job = Some(SweepRequest::new(request_id, cancel, next));
        emit(WorkerEvent::RunProgress(RunProgress::Acquiring));
    }
}

fn fail<T: kcsdi_core::transport::Transport>(
    error: kcsdi_core::Error,
    context: &str,
    device: &mut Option<Device<T>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    discard_job(job);
    let message = format!("{context}: {error}");
    if connection_failed(&error) || device.as_ref().is_some_and(Device::requires_reconnect) {
        if let Some(mut dev) = device.take() {
            dev.close();
        }
        emit(WorkerEvent::ConnectionLost(message));
    } else if !matches!(error, kcsdi_core::Error::Cancelled) {
        emit(WorkerEvent::Error(message));
    }
}

fn connection_failed(error: &kcsdi_core::Error) -> bool {
    matches!(
        error,
        kcsdi_core::Error::Io(_)
            | kcsdi_core::Error::NotConnected
            | kcsdi_core::Error::Timeout
            | kcsdi_core::Error::Protocol(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::tests::{s11, spec};
    use crate::acquisition::{AcquisitionGroup, TraceId};
    use kcsdi_core::Error;
    use std::cell::RefCell;

    fn plan(settings: AcquisitionSettings) -> SweepPlan {
        SweepPlan::from_requests([(TraceId(1), settings)]).unwrap()
    }

    fn request(settings: AcquisitionSettings) -> SweepRequest {
        SweepRequest::new(0, CancellationToken::default(), plan(settings))
    }

    #[test]
    fn matching_save_ack_starts_the_interval_and_preserves_request_identity() {
        let key = RecordKey {
            session_id: 2,
            request_id: 7,
            pass_id: 1,
        };
        let mut recording = RecordingState {
            pending: Some(key),
            ..Default::default()
        };
        let mut current = request(AcquisitionSettings::S11(s11()));
        current.request_id = key.request_id;
        current.phase = RunPhase::Saving;
        current.plan.run.interval_ms = 250;
        let mut job = Some(current);
        let now = Instant::now();
        let events = RefCell::new(Vec::new());
        let path = std::path::PathBuf::from("pass-1.csv");
        recording.accept_result(
            RecordResult {
                key,
                result: Ok(Some(path.clone())),
            },
            WorkerIdentity {
                session_id: 2,
                request_id: 99,
            },
            &mut job,
            now,
            &|event| events.borrow_mut().push(event),
        );
        assert!(recording.pending.is_none());
        let current = job.as_mut().unwrap();
        assert_eq!(current.pass_id, 2);
        let until = now + Duration::from_millis(250);
        assert_eq!(current.phase, RunPhase::Waiting { until });
        assert!(advance_wait(current, false, until - Duration::from_nanos(1)).is_none());
        assert!(advance_wait(current, true, until).is_none());
        assert!(matches!(
            advance_wait(current, false, until),
            Some(RunProgress::Acquiring)
        ));
        let events = events.borrow();
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| (event.session_id, event.request_id) == (2, 7))
        );
        assert!(
            matches!(&events[0].event, WorkerEvent::RunProgress(RunProgress::Saved { path: saved, pass_id: 1 }) if saved == &path)
        );
        assert!(
            matches!(events[1].event, WorkerEvent::RunProgress(RunProgress::Waiting { until: deadline }) if deadline == until)
        );
    }

    #[test]
    fn stale_save_ack_only_releases_the_global_slot() {
        for (session_id, request_id, pass_id) in [(3, 7, 1), (2, 8, 1), (2, 7, 2)] {
            let old = RecordKey {
                session_id: 2,
                request_id: 7,
                pass_id: 1,
            };
            let mut recording = RecordingState {
                pending: Some(old),
                ..Default::default()
            };
            let mut current = request(AcquisitionSettings::S11(s11()));
            current.request_id = request_id;
            current.pass_id = pass_id;
            current.phase = RunPhase::Saving;
            let mut job = Some(current);
            assert!(advance_wait(job.as_mut().unwrap(), true, Instant::now()).is_none());
            recording.accept_result(
                RecordResult {
                    key: old,
                    result: Err("old disk failure".into()),
                },
                WorkerIdentity {
                    session_id,
                    request_id,
                },
                &mut job,
                Instant::now(),
                &|_| panic!("an old acknowledgement must not affect the new request"),
            );
            assert!(recording.pending.is_none());
            let current = job.as_mut().unwrap();
            assert_eq!(
                (current.request_id, current.pass_id, current.next_group),
                (request_id, pass_id, 0)
            );
            assert!(!current.cancel.is_cancelled());
            assert!(matches!(
                advance_wait(current, false, Instant::now()),
                Some(RunProgress::Acquiring)
            ));
        }
    }

    #[test]
    fn stopped_and_failed_recordings_never_restart_acquisition() {
        let key = RecordKey {
            session_id: 2,
            request_id: 7,
            pass_id: 1,
        };
        for stopped in [false, true] {
            let mut recording = RecordingState {
                pending: Some(key),
                ..Default::default()
            };
            let mut current = request(AcquisitionSettings::S11(s11()));
            current.request_id = 7;
            current.phase = RunPhase::Saving;
            let token = current.cancel.clone();
            let mut job = Some(current);
            if stopped {
                discard_job(&mut job);
            }
            let events = RefCell::new(Vec::new());
            recording.accept_result(
                RecordResult {
                    key,
                    result: Err("disk full".into()),
                },
                WorkerIdentity {
                    session_id: 2,
                    request_id: 7,
                },
                &mut job,
                Instant::now(),
                &|event| events.borrow_mut().push(event),
            );
            assert!(recording.pending.is_none());
            assert!(job.is_none());
            assert!(token.is_cancelled());
            let events = events.borrow();
            if stopped {
                assert!(events.is_empty());
            } else {
                assert!(
                    matches!(&events[0].event, WorkerEvent::Error(message) if message == "Recording failed: disk full")
                );
            }
        }
    }

    #[test]
    fn zero_interval_waits_for_a_save_ack_and_pass_overflow_stops() {
        let mut current = request(AcquisitionSettings::S11(s11()));
        current.phase = RunPhase::Saving;
        assert!(advance_wait(&mut current, true, Instant::now()).is_none());
        assert_eq!(current.phase, RunPhase::Saving);
        assert!(matches!(
            current.finish_pass(Instant::now()).unwrap(),
            RunProgress::Acquiring
        ));
        assert_eq!(current.pass_id, 2);
        current.pass_id = u64::MAX;
        assert!(current.finish_pass(Instant::now()).is_err());
    }

    #[test]
    fn recording_progress_drops_on_stop_but_terminal_errors_remain_deliverable() {
        for progress in [
            RunProgress::Acquiring,
            RunProgress::Saving,
            RunProgress::Waiting {
                until: Instant::now(),
            },
            RunProgress::Saved {
                path: "pass.csv".into(),
                pass_id: 1,
            },
        ] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let source = WorkerIdentity {
                session_id: 1,
                request_id: 2,
            };
            sender
                .send(source.event(WorkerEvent::SweepStopped))
                .unwrap();
            let cancel = CancellationToken::default();
            let shutdown = CancellationToken::default();
            let (done, wait) = mpsc::channel();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    send_request_event(
                        &sender,
                        source.event(WorkerEvent::RunProgress(progress)),
                        &shutdown,
                        &cancel,
                        None,
                        &egui::Context::default(),
                    );
                    done.send(()).unwrap();
                });
                assert!(matches!(
                    wait.recv_timeout(Duration::from_millis(30)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                cancel.cancel();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                receiver.recv().unwrap();
                assert!(receiver.try_recv().is_err());
                worker.join().unwrap();
            });
            send_request_event(
                &sender,
                source.event(WorkerEvent::Error("Recording failed: disk full".into())),
                &shutdown,
                &cancel,
                None,
                &egui::Context::default(),
            );
            assert!(matches!(
                receiver.recv().unwrap().event,
                WorkerEvent::Error(_)
            ));
        }
    }

    #[test]
    fn blocked_writer_allows_health_and_releases_the_device_before_joining() {
        use std::io::{BufRead, BufReader, Read, Write};
        const WAIT: Duration = Duration::from_secs(5);
        struct StopOnDrop(CancellationToken, CancellationToken);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
                self.1.cancel();
            }
        }
        for explicit_disconnect in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut plan = plan(AcquisitionSettings::S11(s11()));
            plan.run.recording.enabled = true;
            plan.run.recording.directory = directory.path().to_path_buf();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let (commands, command_rx) = mpsc::channel();
            let (events, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
            let (writing, write_started) = mpsc::channel();
            let (release, wait_release) = mpsc::channel();
            let (closed, device_closed) = mpsc::channel();
            let (done, worker_done) = mpsc::channel();
            let writer = RecordWriter::with_test_save(move |key| {
                writing.send(key).unwrap();
                wait_release
                    .recv_timeout(WAIT)
                    .map_err(|error| error.to_string())?;
                Ok(None)
            })
            .unwrap();
            let shutdown = CancellationToken::default();
            let cancel = CancellationToken::default();
            std::thread::scope(|scope| {
                let _cleanup = StopOnDrop(shutdown.clone(), cancel.clone());
                let server = scope.spawn(move || {
                    let started = Instant::now();
                    let socket = loop {
                        match listener.accept() {
                            Ok((socket, _)) => break socket,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(started.elapsed() < WAIT);
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("accept failed: {error}"),
                        }
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket.set_read_timeout(Some(WAIT)).unwrap();
                    socket.set_write_timeout(Some(WAIT)).unwrap();
                    let mut peer = BufReader::new(socket);
                    let mut byte = [0];
                    peer.read_exact(&mut byte).unwrap();
                    assert_eq!(byte, [b'C']);
                    peer.get_mut().write_all(b"$start,id\n$000000000001\n$end\n").unwrap();
                    for (command, response) in [
                        ("$device\n", "$start,device\n$Synthetic peer\n$<-User @ :replay>\n$<-Software ver:test>\n$<-Hardware ver:test>\n$<-Serial num:000000000001>\n$<-Copyright:Test fixture>\n$end\n"),
                        ("$s11,stop\n", ""), ("$s21,stop\n", ""), ("$spec,stop\n", ""),
                        ("$rfsource,stop\n", ""), ("$afsource,stop\n", ""),
                        ("$s11,init\n", ""), ("$bw,10k\n", ""),
                        ("$s11,run,caloff,z,2,ss,1000000,2000000\n", "$start,s11,z\n$1000000,50,50,0\n$1500000,50,50,0\n$2000000,50,50,0\n$end\n"),
                        ("$temp\n", "$start,temp\n$42\n$end\n"),
                        ("$voltage\n", "$start,voltage\n$12,8\n$end\n"),
                        ("$s11,stop\n", ""), ("$local\n", ""),
                    ] {
                        let mut line = String::new();
                        assert_ne!(peer.read_line(&mut line).unwrap(), 0);
                        assert_eq!(line, command);
                        peer.get_mut().write_all(response.as_bytes()).unwrap();
                    }
                    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
                    closed.send(()).unwrap();
                });
                let worker_shutdown = shutdown.clone();
                let worker = scope.spawn(move || {
                    run_worker(
                        command_rx,
                        events,
                        egui::Context::default(),
                        worker_shutdown,
                        PreviewMailbox::default(),
                        RecordingState {
                            writer: Some(writer),
                            pending: None,
                        },
                    );
                    done.send(()).unwrap();
                });
                commands
                    .send(CommandEnvelope {
                        session_id: 1,
                        request_id: 1,
                        cancel: CancellationToken::default(),
                        command: WorkerCommand::Connect {
                            target: ConnectionTarget::Tcp {
                                host: "127.0.0.1".into(),
                                port,
                            },
                        },
                    })
                    .unwrap();
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::Connected(_)
                ));
                commands
                    .send(CommandEnvelope {
                        session_id: 1,
                        request_id: 2,
                        cancel: cancel.clone(),
                        command: WorkerCommand::RunWorkspace(plan),
                    })
                    .unwrap();
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::RunProgress(RunProgress::Acquiring)
                ));
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::SweepTrace(_)
                ));
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::RunProgress(RunProgress::Saving)
                ));
                assert_eq!(
                    write_started.recv_timeout(WAIT).unwrap(),
                    RecordKey {
                        session_id: 1,
                        request_id: 2,
                        pass_id: 1
                    }
                );
                commands
                    .send(CommandEnvelope {
                        session_id: 1,
                        request_id: 1,
                        cancel: CancellationToken::default(),
                        command: WorkerCommand::RefreshStatus,
                    })
                    .unwrap();
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::Status(_)
                ));
                if explicit_disconnect {
                    commands
                        .send(CommandEnvelope {
                            session_id: 1,
                            request_id: 3,
                            cancel: CancellationToken::default(),
                            command: WorkerCommand::StopSweep,
                        })
                        .unwrap();
                    assert!(matches!(
                        event_rx.recv_timeout(WAIT).unwrap().event,
                        WorkerEvent::SweepStopped
                    ));
                    commands
                        .send(CommandEnvelope {
                            session_id: 2,
                            request_id: 4,
                            cancel: CancellationToken::default(),
                            command: WorkerCommand::Disconnect,
                        })
                        .unwrap();
                    assert!(matches!(
                        event_rx.recv_timeout(WAIT).unwrap().event,
                        WorkerEvent::Disconnected
                    ));
                }
                shutdown.cancel();
                device_closed.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(cancel.is_cancelled());
                assert!(matches!(
                    worker_done.recv_timeout(Duration::from_millis(30)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                release.send(()).unwrap();
                worker_done.recv_timeout(WAIT).unwrap();
                worker.join().unwrap();
                server.join().unwrap();
            });
        }
    }

    fn replay_recorded_passes(fail_second_group: bool) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{TcpListener, TcpStream};
        const WAIT: Duration = Duration::from_secs(5);
        const IDENTITY: &[u8] = b"$start,device\n$Synthetic peer\n$<-User @ :replay>\n$<-Software ver:test>\n$<-Hardware ver:test>\n$<-Serial num:000000000001>\n$<-Copyright:Test fixture>\n$end\n";

        fn expect(peer: &mut BufReader<TcpStream>, command: &str) {
            let mut line = String::new();
            assert_ne!(peer.read_line(&mut line).unwrap(), 0);
            assert_eq!(line, command);
        }

        struct StopOnDrop {
            shutdown: CancellationToken,
            request: CancellationToken,
        }
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.request.cancel();
                self.shutdown.cancel();
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let mut plan = SweepPlan::from_requests([
            (TraceId(1), AcquisitionSettings::S11(s11())),
            (TraceId(2), AcquisitionSettings::Spec(spec())),
            (TraceId(3), AcquisitionSettings::S11(s11())),
        ])
        .unwrap();
        plan.run.interval_ms = 400;
        plan.run.recording.enabled = true;
        plan.run.recording.directory = directory.path().to_path_buf();
        plan.run.recording.format = crate::run_settings::RecordingFormat::Csv;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (commands, command_rx) = mpsc::channel();
        // Rendezvous delivery lets the fixture queue health before accepting
        // Waiting, without relying on the test thread beating the interval.
        let (events, event_rx) = mpsc::sync_channel(0);
        let (deadlines, next_deadline) = mpsc::channel();
        let shutdown = CancellationToken::default();
        let cancel = CancellationToken::default();
        std::thread::scope(|scope| {
            let cleanup = StopOnDrop {
                shutdown: shutdown.clone(),
                request: cancel.clone(),
            };
            let server = scope.spawn(move || {
                let started = Instant::now();
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(started.elapsed() < WAIT);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("peer accept failed: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                socket.set_nodelay(true).unwrap();
                let mut peer = BufReader::new(socket);
                let mut first = [0];
                peer.read_exact(&mut first).unwrap();
                assert_eq!(first, [b'C']);
                peer.get_mut().write_all(b"$start,id\n$000000000001\n$end\n").unwrap();
                expect(&mut peer, "$device\n");
                peer.get_mut().write_all(IDENTITY).unwrap();

                let passes = if fail_second_group { 1 } else { 2 };
                for pass in 1..=passes {
                    if pass == 1 {
                        for command in ["$s11,stop\n", "$s21,stop\n", "$spec,stop\n", "$rfsource,stop\n", "$afsource,stop\n"] {
                            expect(&mut peer, command);
                        }
                    } else {
                        let until = next_deadline.recv_timeout(WAIT).unwrap();
                        expect(&mut peer, "$spec,stop\n");
                        assert!(Instant::now() >= until, "a new pass started before its post-save interval");
                    }
                    for command in ["$s11,init\n", "$bw,10k\n", "$s11,run,caloff,z,2,ss,1000000,2000000\n"] {
                        expect(&mut peer, command);
                    }
                    let value = pass * 10;
                    peer.get_mut().write_all(format!("$start,s11,z\n$1000000,{value},{value},0\n$1500000,{value},{value},0\n$2000000,{value},{value},0\n$end\n").as_bytes()).unwrap();
                    for command in ["$s11,stop\n", "$spec,init\n", "$bw,10k\n", "$specref,-10\n", "$spec,run,caloff,highlo,2,ss,1000000,2000000\n"] {
                        expect(&mut peer, command);
                    }
                    if fail_second_group {
                        peer.get_mut().write_all(b"$start,err_par5\n$invalid setting\n$end\n").unwrap();
                        expect(&mut peer, "$spec,stop\n");
                    } else {
                        peer.get_mut().write_all(format!("$start,spec\n$1000000,-{value}\n$1500000,-{value}\n$2000000,-{value}\n$end\n").as_bytes()).unwrap();
                    }
                    // Status queries are legal while waiting for the next pass
                    // and remain available after a nonfatal acquisition error.
                    expect(&mut peer, "$temp\n");
                    peer.get_mut().write_all(b"$start,temp\n$42\n$end\n").unwrap();
                    expect(&mut peer, "$voltage\n");
                    peer.get_mut().write_all(b"$start,voltage\n$12,8\n$end\n").unwrap();
                }
                if !fail_second_group {
                    expect(&mut peer, "$spec,stop\n");
                }
                expect(&mut peer, "$local\n");
                assert_eq!(peer.read(&mut [0]).unwrap(), 0);
            });
            let worker_shutdown = shutdown.clone();
            let worker = scope.spawn(move || {
                device_worker(
                    command_rx,
                    events,
                    egui::Context::default(),
                    worker_shutdown,
                    PreviewMailbox::default(),
                )
            });
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 1,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::Connect {
                        target: ConnectionTarget::Tcp {
                            host: "127.0.0.1".into(),
                            port,
                        },
                    },
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::Connected(_)
            ));
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 2,
                    cancel: cancel.clone(),
                    command: WorkerCommand::RunWorkspace(plan),
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::RunProgress(RunProgress::Acquiring)
            ));
            let mut paths = Vec::new();
            let passes = if fail_second_group { 1 } else { 2 };
            for pass in 1..=passes {
                for members in [vec![TraceId(1), TraceId(3)], vec![TraceId(2)]] {
                    let event = event_rx.recv_timeout(WAIT).unwrap();
                    assert_eq!((event.session_id, event.request_id), (1, 2));
                    if fail_second_group && members == [TraceId(2)] {
                        assert!(
                            matches!(&event.event, WorkerEvent::Error(error) if error.contains("err_par5"))
                        );
                    } else {
                        let WorkerEvent::SweepTrace(delivery) = event.event else {
                            panic!("unexpected event: {:?}", event.event)
                        };
                        assert_eq!(delivery.members, members);
                        assert_eq!(delivery.snapshot.data.points.len(), 3);
                    }
                }
                if !fail_second_group {
                    assert!(matches!(
                        event_rx.recv_timeout(WAIT).unwrap().event,
                        WorkerEvent::RunProgress(RunProgress::Saving)
                    ));
                    let saved = event_rx.recv_timeout(WAIT).unwrap();
                    let WorkerEvent::RunProgress(RunProgress::Saved { path, pass_id }) =
                        saved.event
                    else {
                        panic!("unexpected event: {:?}", saved.event)
                    };
                    assert_eq!(pass_id, pass);
                    assert!(path.is_file());
                    let root = directory.path().canonicalize().unwrap();
                    assert_ne!(path.parent().unwrap(), root);
                    assert!(path.starts_with(root));
                    paths.push(path);
                    commands
                        .send(CommandEnvelope {
                            session_id: 1,
                            request_id: 1,
                            cancel: CancellationToken::default(),
                            command: WorkerCommand::RefreshStatus,
                        })
                        .unwrap();
                    let waiting = event_rx.recv_timeout(WAIT).unwrap();
                    let WorkerEvent::RunProgress(RunProgress::Waiting { until }) = waiting.event
                    else {
                        panic!("unexpected event: {:?}", waiting.event)
                    };
                    deadlines.send(until).unwrap();
                }
                if fail_second_group {
                    commands
                        .send(CommandEnvelope {
                            session_id: 1,
                            request_id: 1,
                            cancel: CancellationToken::default(),
                            command: WorkerCommand::RefreshStatus,
                        })
                        .unwrap();
                }
                assert!(matches!(
                    event_rx.recv_timeout(WAIT).unwrap().event,
                    WorkerEvent::Status(_)
                ));
                if pass < passes {
                    assert!(matches!(
                        event_rx.recv_timeout(WAIT).unwrap().event,
                        WorkerEvent::RunProgress(RunProgress::Acquiring)
                    ));
                }
            }
            cancel.cancel();
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 3,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::StopSweep,
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::SweepStopped
            ));
            assert!(cancel.is_cancelled());
            commands
                .send(CommandEnvelope {
                    session_id: 2,
                    request_id: 4,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::Disconnect,
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::Disconnected
            ));
            drop(cleanup);
            worker.join().unwrap();
            server.join().unwrap();
            if fail_second_group {
                assert!(paths.is_empty());
                assert!(
                    std::fs::read_dir(directory.path())
                        .unwrap()
                        .next()
                        .is_none()
                );
            } else {
                assert_eq!(paths.len(), 2);
                assert_eq!(paths[0].parent(), paths[1].parent());
                for (index, path) in paths.iter().enumerate() {
                    let mut reader = csv::Reader::from_path(path).unwrap();
                    let rows = reader.records().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(rows.len(), 21);
                    assert_eq!(
                        rows.iter()
                            .map(|row| row[0].to_owned())
                            .collect::<std::collections::BTreeSet<_>>(),
                        ["1".into(), "2".into(), "3".into()].into()
                    );
                    let expected = ((index + 1) * 10).to_string();
                    let resistance = rows
                        .iter()
                        .filter(|row| &row[3] == "resistance_ohm")
                        .collect::<Vec<_>>();
                    assert_eq!(resistance.len(), 6);
                    assert!(resistance.iter().all(|row| row[4] == expected));
                }
            }
        });
    }

    #[test]
    fn recording_replays_full_passes_intervals_status_stop_and_disconnect() {
        replay_recorded_passes(false);
    }

    #[test]
    fn failed_incomplete_pass_never_creates_a_recording_directory() {
        replay_recorded_passes(true);
    }

    struct ListTransport {
        lines: std::collections::VecDeque<String>,
        sent: Arc<std::sync::Mutex<Vec<u8>>>,
        reads: Arc<std::sync::atomic::AtomicUsize>,
        cancel_on_read: Option<(usize, CancellationToken)>,
    }

    impl ListTransport {
        fn new() -> Self {
            Self {
                lines: std::collections::VecDeque::new(),
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                cancel_on_read: None,
            }
        }

        fn queue_point(
            &mut self,
            settings: &kcsdi_core::device::PointSettings,
            frequency: f64,
            value: &str,
        ) {
            let header = if settings.format().is_empty() {
                format!("$start,{}", settings.mode().name())
            } else {
                format!("$start,{},{}", settings.mode().name(), settings.format())
            };
            self.lines
                .extend([header, format!("${frequency},{value}"), "$end".into()]);
            self.lines.extend(
                [
                    "$start,device",
                    "$Synthetic peer",
                    "$<-User @ :replay>",
                    "$<-Software ver:test>",
                    "$<-Hardware ver:test>",
                    "$<-Serial num:000000000001>",
                    "$<-Copyright:Test fixture>",
                    "$end",
                ]
                .map(str::to_owned),
            );
        }
    }

    impl kcsdi_core::transport::Transport for ListTransport {
        fn send_with_timeout(&mut self, data: &[u8], _: Duration) -> kcsdi_core::Result<()> {
            self.sent.lock().unwrap().extend_from_slice(data);
            Ok(())
        }

        fn recv_line(&mut self, _: Duration) -> kcsdi_core::Result<String> {
            let count = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if let Some((read, token)) = &self.cancel_on_read
                && *read == count
            {
                token.cancel();
            }
            self.lines.pop_front().ok_or(Error::NotConnected)
        }
    }

    fn list_settings() -> [kcsdi_core::device::PointSettings; 3] {
        use kcsdi_core::commands::{Cal, Format, Lo};
        use kcsdi_core::device::PointSettings;
        use kcsdi_core::model::Rbw;
        [
            PointSettings::S11 {
                cal: Cal::CalOff,
                format: Format::Loss,
                rbw: None,
            },
            PointSettings::S21 {
                cal: Cal::CalOff,
                format: Format::Loss,
                lo: Lo::HighLo,
                rbw: None,
            },
            PointSettings::Spec {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                rbw: Rbw::R10k,
                ref_level_dbm: -10,
            },
        ]
    }

    #[test]
    fn list_acquisition_keeps_actual_hz_and_repeated_targets_after_each_fence() {
        for settings in list_settings() {
            let requested = [1_000_000, 1_000_000, 4_000_000];
            // Independent replies at a repeated target need not round to the
            // same value or arrive in increasing reported-frequency order.
            let actual = [1_000_000.25, 999_999.0, 4_000_000.2];
            let mut transport = ListTransport::new();
            for (frequency, value) in actual.into_iter().zip(["1", "2", "3"]) {
                transport.queue_point(&settings, frequency, value);
            }
            let sent = transport.sent.clone();
            let reads = transport.reads.clone();
            let mut device = Device::new(transport);
            let mut prefixes = Vec::new();
            let data = measure_list(
                &mut device,
                &settings,
                &requested,
                &CancellationToken::default(),
                |prefix| {
                    assert_eq!(prefix.mode, settings.mode());
                    assert_eq!(prefix.format, settings.format());
                    assert_eq!(prefix.expected_points, 3);
                    assert_eq!(
                        reads.load(std::sync::atomic::Ordering::SeqCst),
                        prefix.points.len() * 11
                    );
                    prefixes.push(
                        prefix
                            .points
                            .iter()
                            .map(|point| point.freq_hz)
                            .collect::<Vec<_>>(),
                    );
                },
            )
            .unwrap();
            assert_eq!(
                prefixes,
                [
                    vec![],
                    actual[..1].to_vec(),
                    actual[..2].to_vec(),
                    actual.to_vec()
                ]
            );
            assert_eq!(
                data.points
                    .iter()
                    .map(|point| point.freq_hz)
                    .collect::<Vec<_>>(),
                actual
            );
            assert_eq!(
                data.points
                    .iter()
                    .map(|point| point.values[0])
                    .collect::<Vec<_>>(),
                [1.0, 2.0, 3.0]
            );
            assert!(!device.requires_reconnect());
            let sent = String::from_utf8(sent.lock().unwrap().clone()).unwrap();
            assert_eq!(sent.matches(",1,ss,1000000\n").count(), 2);
            assert_eq!(sent.matches(",1,ss,4000000\n").count(), 1);
            assert_eq!(sent.matches("\x03$device\n").count(), 3);
        }
    }

    #[test]
    fn list_partial_failure_keeps_only_fenced_previews_and_never_starts_the_next_point() {
        for settings in list_settings() {
            let mut transport = ListTransport::new();
            transport.queue_point(&settings, 999_999.0, "1");
            transport.queue_point(&settings, 2_000_000.0, "2,3");
            let sent = transport.sent.clone();
            let mut device = Device::new(transport);
            let mut prefixes = Vec::new();
            let result = measure_list(
                &mut device,
                &settings,
                &[1_000_000, 2_000_000, 3_000_000],
                &CancellationToken::default(),
                |prefix| prefixes.push(prefix.points.len()),
            );
            assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
            assert_eq!(prefixes, [0, 1]);
            assert!(device.requires_reconnect());
            let sent = String::from_utf8(sent.lock().unwrap().clone()).unwrap();
            assert_eq!(sent.matches(",run,").count(), 2);
            assert!(!sent.contains(",ss,3000000\n"));
            assert_eq!(sent.matches("\x03$device\n").count(), 2);
        }
    }

    #[test]
    fn list_cancellation_after_a_fenced_preview_starts_no_further_point() {
        let settings = list_settings()[0].clone();
        for cancel_during_second_point in [false, true] {
            let cancel = CancellationToken::default();
            let mut transport = ListTransport::new();
            transport.queue_point(&settings, 999_999.0, "1");
            transport.queue_point(&settings, 2_000_000.0, "2");
            if cancel_during_second_point {
                transport.cancel_on_read = Some((13, cancel.clone()));
            }
            let sent = transport.sent.clone();
            let mut device = Device::new(transport);
            let mut prefixes = Vec::new();
            let result = measure_list(
                &mut device,
                &settings,
                &[1_000_000, 2_000_000, 3_000_000],
                &cancel,
                |prefix| {
                    prefixes.push(prefix.points.len());
                    if prefix.points.len() == 1 && !cancel_during_second_point {
                        cancel.cancel();
                    }
                },
            );
            assert!(matches!(result, Err(Error::Cancelled)));
            assert_eq!(prefixes, [0, 1]);
            assert!(!device.requires_reconnect());
            let sent = String::from_utf8(sent.lock().unwrap().clone()).unwrap();
            let expected_runs = if cancel_during_second_point { 2 } else { 1 };
            assert_eq!(sent.matches(",run,").count(), expected_runs);
            assert_eq!(sent.matches("\x03$device\n").count(), expected_runs);
            assert!(!sent.contains(",ss,3000000\n"));
        }
    }

    #[test]
    fn invalid_workspace_plans_send_nothing_on_an_existing_connection() {
        use std::io::Read;
        use std::net::TcpListener;

        let valid = plan(AcquisitionSettings::S11(s11()));
        let mut invalid_params = spec();
        invalid_params.points = 0;
        let invalid_plans = [
            SweepPlan {
                groups: Vec::new(),
                run: Default::default(),
            },
            SweepPlan {
                groups: vec![
                    valid.groups[0].clone(),
                    AcquisitionGroup {
                        settings: AcquisitionSettings::Spec(invalid_params),
                        members: vec![TraceId(2)],
                    },
                ],
                run: Default::default(),
            },
            SweepPlan {
                groups: vec![valid.groups[0].clone(), valid.groups[0].clone()],
                run: Default::default(),
            },
        ];
        for invalid in invalid_plans {
            assert!(invalid.validate().is_err());
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let transport =
                TcpTransport::connect("127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut device = Some(Device::new(transport.into()));
            let mut job = Some(request(AcquisitionSettings::S11(s11())));
            let events = RefCell::new(Vec::new());
            start_sweep(
                invalid,
                2,
                CancellationToken::default(),
                &mut device,
                &mut job,
                &|event| events.borrow_mut().push(event),
            );
            assert!(job.is_none());
            assert!(!device.as_ref().unwrap().requires_reconnect());
            let events = events.borrow();
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], WorkerEvent::Error(_)));
            let error = peer.read(&mut [0]).unwrap_err();
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ));
        }
    }

    #[test]
    fn workspace_replays_group_fanout_round_robin_order_and_partial_cancellation() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{TcpListener, TcpStream};

        const WAIT: Duration = Duration::from_secs(5);
        const IDENTITY: &[u8] = b"$start,device\n\
            $Synthetic peer\n\
            $<-User @ :replay>\n\
            $<-Software ver:test>\n\
            $<-Hardware ver:test>\n\
            $<-Serial num:000000000001>\n\
            $<-Copyright:Test fixture>\n\
            $end\n";

        fn expect_line(peer: &mut BufReader<TcpStream>, expected: &str) {
            let mut line = String::new();
            assert_ne!(peer.read_line(&mut line).unwrap(), 0);
            assert_eq!(line, expected);
        }

        struct ShutdownOnDrop {
            sender: mpsc::Sender<CommandEnvelope>,
            shutdown: CancellationToken,
            request: CancellationToken,
        }

        impl Drop for ShutdownOnDrop {
            fn drop(&mut self) {
                self.request.cancel();
                self.shutdown.cancel();
                let _ = self.sender.send(CommandEnvelope {
                    session_id: 2,
                    request_id: 4,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::Shutdown,
                });
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (commands, command_rx) = mpsc::channel();
        let (events, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let (partial_sent, partial_ready) = mpsc::channel();
        let (frame_sent, frame_ready) = mpsc::channel();
        let (refresh_queued, refresh_ready) = mpsc::channel();
        let shutdown = CancellationToken::default();
        let worker_shutdown = shutdown.clone();
        let cancel = CancellationToken::default();
        let settings = AcquisitionSettings::S11(s11());
        let spectrum = AcquisitionSettings::Spec(spec());
        let transmission = AcquisitionSettings::S21(crate::acquisition::tests::s21());
        let plan = SweepPlan::from_requests([
            (TraceId(1), settings.clone()),
            (TraceId(2), spectrum.clone()),
            (TraceId(3), settings.clone()),
            (TraceId(4), transmission.clone()),
        ])
        .unwrap();
        std::thread::scope(|scope| {
            let cleanup = ShutdownOnDrop {
                sender: commands.clone(),
                shutdown,
                request: cancel.clone(),
            };
            let server = scope.spawn(move || {
                let began = Instant::now();
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(began.elapsed() < WAIT, "worker did not connect");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("peer accept failed: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                socket.set_nodelay(true).unwrap();
                let mut peer = BufReader::new(socket);
                let mut handshake = [0];
                peer.read_exact(&mut handshake).unwrap();
                assert_eq!(&handshake, b"C");
                peer.get_mut()
                    .write_all(b"$start,id\n$000000000001\n$end\n")
                    .unwrap();
                expect_line(&mut peer, "$device\n");
                peer.get_mut().write_all(IDENTITY).unwrap();

                for command in [
                    "$s11,stop\n",
                    "$s21,stop\n",
                    "$spec,stop\n",
                    "$rfsource,stop\n",
                    "$afsource,stop\n",
                    "$s11,init\n",
                    "$bw,10k\n",
                    "$s11,run,caloff,z,2,ss,1000000,2000000\n",
                ] {
                    expect_line(&mut peer, command);
                }
                peer.get_mut()
                    .write_all(b"$start,s11,z\n$1000000,50,50,0\n")
                    .unwrap();
                frame_sent.send(()).unwrap();
                refresh_ready.recv_timeout(WAIT).unwrap();
                peer.get_mut()
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let error = peer.read(&mut [0]).unwrap_err();
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ));
                peer.get_mut().set_read_timeout(Some(WAIT)).unwrap();
                peer.get_mut()
                    .write_all(b"$1500000,55,55,0\n$2000000,60,60,0\n$end\n")
                    .unwrap();
                expect_line(&mut peer, "$temp\n");
                let temperature_response_at = Instant::now();
                peer.get_mut()
                    .write_all(b"$start,temp\n$42\n$end\n")
                    .unwrap();
                expect_line(&mut peer, "$voltage\n");
                assert!(temperature_response_at.elapsed() >= STATUS_COMMAND_GAP);
                peer.get_mut()
                    .write_all(b"$start,voltage\n$12,8\n$end\n")
                    .unwrap();
                for command in [
                    "$s11,stop\n",
                    "$spec,init\n",
                    "$bw,10k\n",
                    "$specref,-10\n",
                    "$spec,run,caloff,highlo,2,ss,1000000,2000000\n",
                ] {
                    expect_line(&mut peer, command);
                }
                peer.get_mut()
                    .write_all(b"$start,spec\n$1000000,-10\n$1500000,-20\n$2000000,-30\n$end\n")
                    .unwrap();
                for command in [
                    "$spec,stop\n",
                    "$s21,init\n",
                    "$bw,10k\n",
                    "$s21,run,caloff,delay,highlo,2,ss,1000000,2000000\n",
                ] {
                    expect_line(&mut peer, command);
                }
                peer.get_mut()
                    .write_all(
                        b"$start,s21,delay\n$1000000,-5e-9\n$1500000,0\n$2000000,8e-9\n$end\n",
                    )
                    .unwrap();
                // The next pass returns to the first group, rather than
                // measuring its second display member as a separate sweep.
                for command in [
                    "$s21,stop\n",
                    "$s11,init\n",
                    "$bw,10k\n",
                    "$s11,run,caloff,z,2,ss,1000000,2000000\n",
                ] {
                    expect_line(&mut peer, command);
                }
                peer.get_mut()
                    .write_all(b"$start,s11,z\n$1000000,70,70,0\n$1500000,")
                    .unwrap();
                partial_sent.send(()).unwrap();
                let mut interrupt = [0];
                peer.read_exact(&mut interrupt).unwrap();
                assert_eq!(interrupt, [3]);
                expect_line(&mut peer, "$device\n");
                // Ordered local replay, not an assertion about untested
                // firmware behavior after an interrupted command.
                peer.get_mut().write_all(b"75,75,0\n$end\n").unwrap();
                peer.get_mut().write_all(IDENTITY).unwrap();
                expect_line(&mut peer, "$local\n");
                assert_eq!(peer.read(&mut [0]).unwrap(), 0);
            });
            let worker = scope.spawn(move || {
                device_worker(
                    command_rx,
                    events,
                    egui::Context::default(),
                    worker_shutdown,
                    PreviewMailbox::default(),
                );
            });
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 1,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::Connect {
                        target: ConnectionTarget::Tcp {
                            host: "127.0.0.1".into(),
                            port,
                        },
                    },
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::Connected(_)
            ));
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 2,
                    cancel: cancel.clone(),
                    command: WorkerCommand::RunWorkspace(plan),
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::RunProgress(RunProgress::Acquiring)
            ));
            frame_ready.recv_timeout(WAIT).unwrap();
            // Both requests predate the active acquisition generation. They
            // still belong to this session and must coalesce at its boundary.
            for _ in 0..2 {
                commands
                    .send(CommandEnvelope {
                        session_id: 1,
                        request_id: 1,
                        cancel: CancellationToken::default(),
                        command: WorkerCommand::RefreshStatus,
                    })
                    .unwrap();
            }
            refresh_queued.send(()).unwrap();
            let mut completed = Vec::new();
            for (cycle, members, expected_settings) in [
                (1, vec![TraceId(1), TraceId(3)], settings),
                (2, vec![TraceId(2)], spectrum),
                (3, vec![TraceId(4)], transmission),
            ] {
                let event = event_rx.recv_timeout(WAIT).unwrap();
                assert_eq!(
                    (event.session_id, event.request_id, event.cycle_id),
                    (1, 2, Some(cycle))
                );
                let WorkerEvent::SweepTrace(delivery) = event.event else {
                    panic!("unexpected event: {:?}", event.event);
                };
                assert_eq!(delivery.members, members);
                assert_eq!(delivery.snapshot.settings, expected_settings);
                assert_eq!(delivery.snapshot.session_id, 1);
                assert!(expected_settings.accepts(&delivery.snapshot.data));
                completed.push(delivery.snapshot);
                if cycle == 1 {
                    let status = event_rx.recv_timeout(WAIT).unwrap();
                    assert_eq!((status.session_id, status.request_id), (1, 1));
                    assert!(matches!(
                        status.event,
                        WorkerEvent::Status(crate::health::HealthSnapshot {
                            temperature: 42.0,
                            ..
                        })
                    ));
                }
            }
            partial_ready.recv_timeout(WAIT).unwrap();
            cancel.cancel();
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 3,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::StopSweep,
                })
                .unwrap();
            let stopped = event_rx.recv_timeout(WAIT).unwrap();
            assert_eq!(
                (stopped.session_id, stopped.request_id, stopped.cycle_id),
                (1, 3, None)
            );
            assert!(matches!(stopped.event, WorkerEvent::SweepStopped));
            assert!(event_rx.try_recv().is_err());
            assert_eq!(completed[0].data.points[0].values, [50.0, 50.0, 0.0]);
            assert_eq!(completed[1].data.points[0].values, [-10.0]);
            assert_eq!(completed[2].data.points[0].values, [-5e-9]);
            drop(cleanup);
            worker.join().unwrap();
            server.join().unwrap();
        });
    }

    #[test]
    fn frame_resets_bypass_the_preview_throttle() {
        let now = Instant::now();
        assert!(!preview_due(Some(now), false));
        assert!(preview_due(Some(now), true));
        assert!(preview_due(None, false));
    }

    #[test]
    fn reliable_results_wait_for_capacity_without_being_dropped() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let source = WorkerIdentity {
            session_id: 1,
            request_id: 1,
        };
        sender
            .send(source.event(WorkerEvent::SweepStopped))
            .unwrap();
        let shutdown = CancellationToken::default();
        let worker_shutdown = shutdown.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send_event(
                &sender,
                source.event(WorkerEvent::SweepStopped),
                &worker_shutdown,
                None,
                &egui::Context::default(),
            );
            done_tx.send(()).unwrap();
        });
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        receiver.recv().unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            receiver.recv().unwrap().event,
            WorkerEvent::SweepStopped
        ));
        assert!(!shutdown.is_cancelled());
        worker.join().unwrap();
    }

    #[test]
    fn cancellation_and_shutdown_can_escape_a_full_result_queue() {
        for cancel_request in [false, true] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let source = WorkerIdentity::default();
            sender
                .send(source.event(WorkerEvent::SweepStopped))
                .unwrap();
            let shutdown = CancellationToken::default();
            let request = CancellationToken::default();
            let worker_shutdown = shutdown.clone();
            let worker_request = request.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                send_event(
                    &sender,
                    source.event(WorkerEvent::SweepStopped),
                    &worker_shutdown,
                    Some(&worker_request),
                    &egui::Context::default(),
                );
                done_tx.send(()).unwrap();
            });
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            if cancel_request {
                request.cancel();
            } else {
                shutdown.cancel();
            }
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            receiver.recv().unwrap();
            assert!(receiver.try_recv().is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_missing_ui_stops_reliable_event_delivery() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let shutdown = CancellationToken::default();
        send_event(
            &sender,
            WorkerIdentity::default().event(WorkerEvent::SweepStopped),
            &shutdown,
            None,
            &egui::Context::default(),
        );
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn cancelled_runs_are_skipped_and_stop_acknowledges_its_own_request() {
        let mut identity = WorkerIdentity {
            session_id: 1,
            request_id: 1,
        };
        let mut device = None;
        let mut job = None;
        let events = RefCell::new(Vec::new());
        let token = CancellationToken::default();
        token.cancel();
        handle(
            CommandEnvelope {
                session_id: 1,
                request_id: 2,
                cancel: token,
                command: WorkerCommand::RunWorkspace(plan(AcquisitionSettings::S11(s11()))),
            },
            &mut identity,
            &mut device,
            &mut job,
            &|event| events.borrow_mut().push(event),
        );
        assert!(job.is_none());
        assert!(events.borrow().is_empty());
        handle(
            CommandEnvelope {
                session_id: 1,
                request_id: 3,
                cancel: CancellationToken::default(),
                command: WorkerCommand::StopSweep,
            },
            &mut identity,
            &mut device,
            &mut job,
            &|event| events.borrow_mut().push(event),
        );
        let events = events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request_id, 3);
        assert_eq!(events[0].cycle_id, None);
        assert!(matches!(events[0].event, WorkerEvent::SweepStopped));
    }

    #[test]
    fn shutdown_wakes_and_joins_an_idle_worker() {
        let (sender, receiver) = mpsc::channel();
        let (events, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let shutdown = CancellationToken::default();
        let worker_shutdown = shutdown.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            device_worker(
                receiver,
                events,
                egui::Context::default(),
                worker_shutdown,
                PreviewMailbox::default(),
            );
            done_tx.send(()).unwrap();
        });
        shutdown.cancel();
        let _ = sender.send(CommandEnvelope {
            session_id: 1,
            request_id: 1,
            cancel: CancellationToken::default(),
            command: WorkerCommand::Shutdown,
        });
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn stale_commands_cannot_replace_the_session_or_its_request() {
        let mut identity = WorkerIdentity {
            session_id: 4,
            request_id: 9,
        };
        let mut device = None;
        let mut job = None;
        let events = RefCell::new(Vec::new());
        let emit = |event| events.borrow_mut().push(event);
        for (session_id, request_id, command) in [
            (3, 10, WorkerCommand::Disconnect),
            (
                4,
                8,
                WorkerCommand::RunWorkspace(plan(AcquisitionSettings::S11(s11()))),
            ),
            (
                3,
                10,
                WorkerCommand::RunWorkspace(plan(AcquisitionSettings::Spec(spec()))),
            ),
            (3, 10, WorkerCommand::RefreshStatus),
            (
                3,
                10,
                WorkerCommand::Connect {
                    target: ConnectionTarget::Tcp {
                        host: "127.0.0.1".into(),
                        port: 0,
                    },
                },
            ),
        ] {
            handle(
                CommandEnvelope {
                    session_id,
                    request_id,
                    cancel: CancellationToken::default(),
                    command,
                },
                &mut identity,
                &mut device,
                &mut job,
                &emit,
            );
            assert_eq!((identity.session_id, identity.request_id), (4, 9));
            assert!(device.is_none());
            assert!(job.is_none());
            assert!(events.borrow().is_empty());
        }
        handle(
            CommandEnvelope {
                session_id: 5,
                request_id: 10,
                cancel: CancellationToken::default(),
                command: WorkerCommand::Disconnect,
            },
            &mut identity,
            &mut device,
            &mut job,
            &emit,
        );
        assert_eq!((identity.session_id, identity.request_id), (5, 10));
        assert_eq!(events.borrow().len(), 1);
        let events = events.borrow();
        assert_eq!((events[0].session_id, events[0].request_id), (5, 10));
        assert!(matches!(events[0].event, WorkerEvent::Disconnected));
    }

    #[test]
    fn run_without_a_connection_cannot_create_a_dormant_job() {
        for command in [
            WorkerCommand::RunWorkspace(plan(AcquisitionSettings::S11(s11()))),
            WorkerCommand::RunWorkspace(plan(AcquisitionSettings::Spec(spec()))),
            WorkerCommand::RunWorkspace(plan(AcquisitionSettings::S21(
                crate::acquisition::tests::s21(),
            ))),
        ] {
            let mut identity = WorkerIdentity {
                session_id: 1,
                request_id: 1,
            };
            let mut device = None;
            let mut job = None;
            let events = RefCell::new(Vec::new());
            handle(
                CommandEnvelope {
                    session_id: 1,
                    request_id: 2,
                    cancel: CancellationToken::default(),
                    command,
                },
                &mut identity,
                &mut device,
                &mut job,
                &|event| events.borrow_mut().push(event),
            );
            assert!(job.is_none());
            assert_eq!(identity.request_id, 2);
            let events = events.borrow();
            assert_eq!((events[0].session_id, events[0].request_id), (1, 2));
            assert!(matches!(events[0].event, WorkerEvent::ConnectionLost(_)));
        }
    }

    #[test]
    fn status_refresh_preserves_the_repeating_jobs_request_identity() {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(socket);
            for (command, response) in [
                ("$temp\n", "$start,temp\n$42\n$end\n"),
                ("$voltage\n", "$start,voltage\n$12,8\n$end\n"),
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, command);
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        let mut device = Some(Device::new(
            TcpTransport::connect("127.0.0.1", port).unwrap().into(),
        ));
        let mut identity = WorkerIdentity {
            session_id: 2,
            request_id: 7,
        };
        let mut job = Some(SweepRequest {
            request_id: 7,
            cancel: CancellationToken::default(),
            plan: SweepPlan::from_requests([
                (TraceId(1), AcquisitionSettings::S11(s11())),
                (TraceId(2), AcquisitionSettings::Spec(spec())),
            ])
            .unwrap(),
            next_group: 1,
            pass_id: 1,
            pass: Vec::new(),
            phase: RunPhase::Acquiring,
        });
        let events = RefCell::new(Vec::new());
        handle(
            CommandEnvelope {
                session_id: 2,
                request_id: 3,
                cancel: CancellationToken::default(),
                command: WorkerCommand::RefreshStatus,
            },
            &mut identity,
            &mut device,
            &mut job,
            &|event| events.borrow_mut().push(event),
        );
        server.join().unwrap();
        assert_eq!((identity.session_id, identity.request_id), (2, 7));
        let job = job.unwrap();
        assert_eq!(job.request_id, 7);
        assert_eq!(job.next_group, 1);
        assert_eq!(job.plan.groups.len(), 2);
        assert!(device.is_some());
        let events = events.borrow();
        assert_eq!((events[0].session_id, events[0].request_id), (2, 3));
        assert!(matches!(
            events[0].event,
            WorkerEvent::Status(crate::health::HealthSnapshot {
                temperature: 42.0,
                ..
            })
        ));
    }

    #[test]
    fn deferred_refreshes_coalesce_behind_controls_and_expire_with_the_session() {
        let mut identity = WorkerIdentity {
            session_id: 2,
            request_id: 7,
        };
        let mut device = None;
        let mut job = Some(request(AcquisitionSettings::S11(s11())));
        let mut pending = None;
        let events = RefCell::new(Vec::new());
        let emit = |event| events.borrow_mut().push(event);
        for request_id in [3, 4] {
            handle_or_defer(
                CommandEnvelope {
                    session_id: 2,
                    request_id,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::RefreshStatus,
                },
                &mut pending,
                &mut identity,
                &mut device,
                &mut job,
                false,
                &emit,
            );
        }
        assert_eq!(pending.as_ref().unwrap().request_id, 4);
        assert!(job.is_some());
        assert!(events.borrow().is_empty());
        handle_or_defer(
            CommandEnvelope {
                session_id: 2,
                request_id: 8,
                cancel: CancellationToken::default(),
                command: WorkerCommand::StopSweep,
            },
            &mut pending,
            &mut identity,
            &mut device,
            &mut job,
            false,
            &emit,
        );
        assert!(job.is_none());
        assert!(matches!(
            events.borrow()[0].event,
            WorkerEvent::SweepStopped
        ));
        assert_eq!(pending.as_ref().unwrap().request_id, 4);
        assert_eq!(identity.request_id, 8);
        handle_or_defer(
            CommandEnvelope {
                session_id: 1,
                request_id: 9,
                cancel: CancellationToken::default(),
                command: WorkerCommand::RefreshStatus,
            },
            &mut pending,
            &mut identity,
            &mut device,
            &mut job,
            false,
            &emit,
        );
        assert_eq!(pending.as_ref().unwrap().request_id, 4);
        handle_or_defer(
            CommandEnvelope {
                session_id: 3,
                request_id: 9,
                cancel: CancellationToken::default(),
                command: WorkerCommand::Disconnect,
            },
            &mut pending,
            &mut identity,
            &mut device,
            &mut job,
            false,
            &emit,
        );
        assert!(pending.is_none());
        assert!(matches!(
            events.borrow()[1].event,
            WorkerEvent::Disconnected
        ));
    }

    #[test]
    fn recoverable_status_failures_preserve_the_job_and_allow_a_fresh_pair() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        for fail_voltage in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut peer = BufReader::new(socket);
                let mut replies = Vec::new();
                if fail_voltage {
                    replies.push(("$temp\n", "$start,temp\n$99\n$end\n"));
                }
                replies.push((
                    if fail_voltage {
                        "$voltage\n"
                    } else {
                        "$temp\n"
                    },
                    "$start,err_par5\n$invalid query\n$end\n",
                ));
                replies.extend([
                    ("$temp\n", "$start,temp\n$42\n$end\n"),
                    ("$voltage\n", "$start,voltage\n$12,8\n$end\n"),
                ]);
                for (command, response) in replies {
                    let mut line = String::new();
                    peer.read_line(&mut line).unwrap();
                    assert_eq!(line, command);
                    peer.get_mut().write_all(response.as_bytes()).unwrap();
                }
            });
            let mut device = Some(Device::new(
                TcpTransport::connect("127.0.0.1", port).unwrap().into(),
            ));
            let mut identity = WorkerIdentity {
                session_id: 2,
                request_id: 7,
            };
            let mut job = Some(SweepRequest {
                request_id: 7,
                cancel: CancellationToken::default(),
                plan: SweepPlan::from_requests([
                    (TraceId(1), AcquisitionSettings::S11(s11())),
                    (TraceId(2), AcquisitionSettings::Spec(spec())),
                ])
                .unwrap(),
                next_group: 1,
                pass_id: 1,
                pass: Vec::new(),
                phase: RunPhase::Acquiring,
            });
            let events = RefCell::new(Vec::new());
            for attempt in 0..2 {
                let started = Instant::now();
                handle(
                    CommandEnvelope {
                        session_id: 2,
                        request_id: 3,
                        cancel: CancellationToken::default(),
                        command: WorkerCommand::RefreshStatus,
                    },
                    &mut identity,
                    &mut device,
                    &mut job,
                    &|event| events.borrow_mut().push(event),
                );
                assert!(device.is_some());
                assert!(!device.as_ref().unwrap().requires_reconnect());
                assert_eq!(job.as_ref().unwrap().request_id, 7);
                assert_eq!(job.as_ref().unwrap().next_group, 1);
                assert_eq!(identity.request_id, 7);
                let events = events.borrow();
                assert_eq!(events.len(), attempt + 1);
                let event = &events[attempt];
                assert_eq!((event.session_id, event.request_id), (2, 3));
                if attempt == 0 {
                    assert!(
                        matches!(&event.event, WorkerEvent::StatusFailed(message) if message.contains("err_par5"))
                    );
                } else {
                    let WorkerEvent::Status(snapshot) = &event.event else {
                        panic!("expected complete status pair")
                    };
                    assert_eq!(snapshot.temperature, 42.0);
                    assert_eq!(snapshot.voltage.external, 12.0);
                    assert_eq!(snapshot.voltage.battery, 8.0);
                    assert!(snapshot.observed_at >= started);
                    assert!(snapshot.observed_at.elapsed() >= STATUS_COMMAND_GAP);
                }
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn cancelled_status_gap_and_obsolete_status_delivery_do_not_block() {
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(matches!(status_command_gap(&cancel), Err(Error::Cancelled)));
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(WorkerIdentity::default().event(WorkerEvent::SweepStopped))
            .unwrap();
        let shutdown = CancellationToken::default();
        send_event(
            &sender,
            WorkerIdentity::default().event(WorkerEvent::StatusFailed("obsolete".into())),
            &shutdown,
            Some(&cancel),
            &egui::Context::default(),
        );
        assert!(!shutdown.is_cancelled());
        assert!(matches!(
            receiver.recv().unwrap().event,
            WorkerEvent::SweepStopped
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn shutdown_interrupts_an_in_flight_status_pair_and_releases_remote_mode() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        const WAIT: Duration = Duration::from_secs(5);
        struct ShutdownOnDrop {
            commands: mpsc::Sender<CommandEnvelope>,
            session: CancellationToken,
            shutdown: CancellationToken,
        }
        impl Drop for ShutdownOnDrop {
            fn drop(&mut self) {
                self.session.cancel();
                self.shutdown.cancel();
                let _ = self.commands.send(CommandEnvelope {
                    session_id: 2,
                    request_id: 2,
                    cancel: CancellationToken::default(),
                    command: WorkerCommand::Shutdown,
                });
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (commands, command_rx) = mpsc::channel();
        let (events, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let (query_sent, query_ready) = mpsc::channel();
        let session = CancellationToken::default();
        let shutdown = CancellationToken::default();
        let worker_shutdown = shutdown.clone();
        std::thread::scope(|scope| {
            let cleanup = ShutdownOnDrop {
                commands: commands.clone(),
                session: session.clone(),
                shutdown,
            };
            let server = scope.spawn(move || {
                let started = Instant::now();
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(started.elapsed() < WAIT);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("peer accept failed: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                let mut peer = BufReader::new(socket);
                let mut byte = [0];
                peer.read_exact(&mut byte).unwrap();
                assert_eq!(&byte, b"C");
                peer.get_mut()
                    .write_all(b"$start,id\n$000000000001\n$end\n")
                    .unwrap();
                let mut line = String::new();
                peer.read_line(&mut line).unwrap();
                assert_eq!(line, "$device\n");
                peer.get_mut()
                    .write_all(
                        b"$start,device\n$Synthetic peer\n\
                    $<-User @ :replay>\n$<-Software ver:test>\n\
                    $<-Hardware ver:test>\n$<-Serial num:000000000001>\n\
                    $<-Copyright:Test fixture>\n$end\n",
                    )
                    .unwrap();
                line.clear();
                peer.read_line(&mut line).unwrap();
                assert_eq!(line, "$temp\n");
                peer.get_mut().write_all(b"$start,temp\n").unwrap();
                query_sent.send(()).unwrap();
                line.clear();
                peer.read_line(&mut line).unwrap();
                assert_eq!(line, "$local\n");
                // Closing with an unread status prefix can reset TCP after local.
                match peer.read(&mut byte) {
                    Ok(0) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                    result => panic!("expected socket closure after local, got {result:?}"),
                }
            });
            let worker = scope.spawn(move || {
                device_worker(
                    command_rx,
                    events,
                    egui::Context::default(),
                    worker_shutdown,
                    PreviewMailbox::default(),
                )
            });
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 1,
                    cancel: session.clone(),
                    command: WorkerCommand::Connect {
                        target: ConnectionTarget::Tcp {
                            host: "127.0.0.1".into(),
                            port,
                        },
                    },
                })
                .unwrap();
            assert!(matches!(
                event_rx.recv_timeout(WAIT).unwrap().event,
                WorkerEvent::Connected(_)
            ));
            commands
                .send(CommandEnvelope {
                    session_id: 1,
                    request_id: 1,
                    cancel: session,
                    command: WorkerCommand::RefreshStatus,
                })
                .unwrap();
            query_ready.recv_timeout(WAIT).unwrap();
            let cancelled_at = Instant::now();
            drop(cleanup);
            worker.join().unwrap();
            assert!(cancelled_at.elapsed() < Duration::from_secs(3));
            server.join().unwrap();
            assert!(event_rx.try_recv().is_err());
        });
    }

    #[test]
    fn fatal_failures_clear_the_job_and_report_connection_loss() {
        for error in [
            Error::Timeout,
            Error::NotConnected,
            Error::Protocol("oversized packet".into()),
            Error::Io(std::io::ErrorKind::ConnectionReset.into()),
        ] {
            let mut device: Option<Device<ConnectionTransport>> = None;
            let mut job = Some(request(AcquisitionSettings::S11(s11())));
            let events = RefCell::new(Vec::new());
            fail(error, "Sweep failed", &mut device, &mut job, &|event| {
                events.borrow_mut().push(event);
            });
            assert!(job.is_none());
            assert!(matches!(events.borrow()[0], WorkerEvent::ConnectionLost(_)));
        }
    }

    #[test]
    fn device_error_packets_do_not_invalidate_the_connection() {
        for error in [
            Error::Device("err_par5".into()),
            Error::InvalidParameter("invalid frequency".into()),
            Error::DeviceBusy("front-panel dialog".into()),
        ] {
            assert!(!connection_failed(&error));
        }
    }

    #[test]
    fn a_failed_status_query_releases_an_existing_connection() {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(socket)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "$temp\n");
        });
        let mut device = Some(Device::new(
            TcpTransport::connect("127.0.0.1", port).unwrap().into(),
        ));
        let mut job = Some(request(AcquisitionSettings::S11(s11())));
        let events = RefCell::new(Vec::new());
        handle(
            CommandEnvelope {
                session_id: 0,
                request_id: 0,
                cancel: CancellationToken::default(),
                command: WorkerCommand::RefreshStatus,
            },
            &mut WorkerIdentity::default(),
            &mut device,
            &mut job,
            &|event| {
                events.borrow_mut().push(event);
            },
        );
        server.join().unwrap();
        assert!(device.is_none());
        assert!(job.is_none());
        assert!(matches!(
            events.borrow()[0].event,
            WorkerEvent::ConnectionLost(_)
        ));
    }

    #[test]
    fn a_device_error_with_failed_cleanup_cannot_keep_a_reusable_session() {
        struct CleanupFailure {
            lines: std::collections::VecDeque<&'static str>,
            sends: usize,
        }
        impl kcsdi_core::transport::Transport for CleanupFailure {
            fn send_with_timeout(&mut self, _: &[u8], _: Duration) -> kcsdi_core::Result<()> {
                self.sends += 1;
                if self.sends == 9 {
                    Err(Error::NotConnected)
                } else {
                    Ok(())
                }
            }
            fn recv_line(&mut self, _: std::time::Duration) -> kcsdi_core::Result<String> {
                self.lines
                    .pop_front()
                    .map(str::to_string)
                    .ok_or(Error::Timeout)
            }
        }
        let mut device = Some(Device::new(CleanupFailure {
            lines: ["$start,err_par5", "$invalid frequency", "$end"].into(),
            sends: 0,
        }));
        let params = s11();
        let error = device.as_mut().unwrap().sweep_s11(&params).unwrap_err();
        assert!(matches!(&error, Error::Device(name) if name == "err_par5"));
        let mut job = Some(request(AcquisitionSettings::S11(params)));
        let events = RefCell::new(Vec::new());
        fail(error, "Sweep failed", &mut device, &mut job, &|event| {
            events.borrow_mut().push(event);
        });
        assert!(device.is_none());
        assert!(job.is_none());
        assert!(matches!(events.borrow()[0], WorkerEvent::ConnectionLost(_)));
    }
}
