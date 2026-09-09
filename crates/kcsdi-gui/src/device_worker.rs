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
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::SweepData;
use kcsdi_core::transport::TcpTransport;
use log::{error, info};

use crate::acquisition::{AcquisitionSettings, CompletedSweep, SweepDelivery, SweepPlan};
use crate::preview::{PreviewEnvelope, PreviewMailbox};
use crate::state::{CommandEnvelope, DEVICE_MODEL, EventEnvelope, WorkerCommand, WorkerEvent};

pub const EVENT_CAPACITY: usize = 16;
const STATUS_COMMAND_GAP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
struct SweepRequest {
    request_id: u64,
    cancel: CancellationToken,
    plan: SweepPlan,
    next_group: usize,
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
    let mut device: Option<Device<TcpTransport>> = None;
    let mut job: Option<SweepRequest> = None;
    let mut identity = WorkerIdentity::default();
    let mut cycle_id = 0_u64;
    let mut pending_status = None;

    let emit = |evt: EventEnvelope| {
        send_event(&evt_tx, evt, &shutdown, None, &ctx);
    };

    'worker: loop {
        if shutdown.is_cancelled() {
            break;
        }
        if device.is_none() || job.is_none() {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if shutdown.is_cancelled() || matches!(cmd.command, WorkerCommand::Shutdown) {
                        break;
                    }
                    handle_or_defer(
                        cmd,
                        &mut pending_status,
                        &mut identity,
                        &mut device,
                        &mut job,
                        &emit,
                    );
                }
                Err(_) => break,
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
                    handle_or_defer(
                        cmd,
                        &mut pending_status,
                        &mut identity,
                        &mut device,
                        &mut job,
                        &emit,
                    );
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'worker,
            }
        }
        if let Some(status) = pending_status.take() {
            let cancel = status.cancel.clone();
            handle(status, &mut identity, &mut device, &mut job, &|event| {
                send_event(&evt_tx, event, &shutdown, Some(&cancel), &ctx);
            });
        }
        if shutdown.is_cancelled() {
            break;
        }
        if device.is_some() && job.is_some() {
            // A stream is owned exclusively until its complete frame or cleanup.
            let current = job.clone().expect("checked above");
            if current.cancel.is_cancelled() {
                job = None;
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
            let progress = |progress: kcsdi_core::device::SweepProgress<'_>| {
                if current.cancel.is_cancelled()
                    || shutdown.is_cancelled()
                    || !preview_due(last_preview, progress.points.is_empty())
                {
                    return;
                }
                last_preview = Some(Instant::now());
                preview.publish(PreviewEnvelope {
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
            let result = match &group.settings {
                AcquisitionSettings::Spec(params) => {
                    dev.sweep_spec_controlled(params, &current.cancel, progress)
                }
                AcquisitionSettings::S11(params) => {
                    dev.sweep_s11_controlled(params, &current.cancel, progress)
                }
                AcquisitionSettings::S21(params) => {
                    dev.sweep_s21_controlled(params, &current.cancel, progress)
                }
            };
            match result {
                Ok(data) => {
                    let mut event = source.event(WorkerEvent::SweepTrace(SweepDelivery {
                        members: group.members.clone(),
                        snapshot: Arc::new(CompletedSweep {
                            data,
                            settings: group.settings.clone(),
                            session_id: source.session_id,
                            completed_at: SystemTime::now(),
                        }),
                    }));
                    event.cycle_id = Some(cycle_id);
                    send_event(&evt_tx, event, &shutdown, Some(&current.cancel), &ctx);
                    if let Some(job) = job.as_mut() {
                        job.next_group = (current.next_group + 1) % current.plan.groups.len();
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
    if let Some(mut dev) = device.take() {
        dev.close();
    }
}

fn handle_or_defer(
    envelope: CommandEnvelope,
    pending_status: &mut Option<CommandEnvelope>,
    identity: &mut WorkerIdentity,
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(EventEnvelope),
) {
    if matches!(envelope.command, WorkerCommand::RefreshStatus) {
        if envelope.session_id == identity.session_id {
            *pending_status = Some(envelope);
        }
    } else {
        handle(envelope, identity, device, job, emit);
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

/// Apply one command to the worker-local connection state.
fn handle(
    envelope: CommandEnvelope,
    identity: &mut WorkerIdentity,
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
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
    match command {
        WorkerCommand::Connect { host, port } => {
            *job = None;
            // Drop an old session before opening the device's single
            // control connection, including reconnect after a failure.
            *device = None;
            match Device::connect_with_model_controlled(&host, port, DEVICE_MODEL, &cancel)
                .and_then(|mut dev| {
                    let info = dev.device_info_controlled(&cancel)?;
                    Ok((dev, info))
                }) {
                Ok((dev, info)) => {
                    info!("connected to {host}:{port}, serial {}", info.serial);
                    *device = Some(dev);
                    emit(WorkerEvent::Connected(info));
                }
                Err(e) => {
                    *device = None;
                    if !matches!(e, kcsdi_core::Error::Cancelled) {
                        error!("connect to {host}:{port} failed: {e}");
                        emit(WorkerEvent::Error(format!("Connect failed: {e}")));
                    }
                }
            }
        }
        WorkerCommand::Disconnect | WorkerCommand::Shutdown => {
            *job = None;
            if let Some(mut dev) = device.take() {
                dev.close();
            }
            emit(WorkerEvent::Disconnected);
        }
        WorkerCommand::RunWorkspace(plan) => {
            start_sweep(plan, request_id, cancel, device, job, &emit);
        }
        WorkerCommand::StopSweep => {
            *job = None;
            if let Some(dev) = device.as_mut()
                && let Err(e) = dev.stop_sweep()
            {
                fail(e, "Stop failed", device, job, &emit);
            } else {
                emit(WorkerEvent::SweepStopped);
            }
        }
        WorkerCommand::RefreshStatus => {
            refresh_status(&cancel, device, job, &emit);
        }
    }
}

fn refresh_status(
    cancel: &CancellationToken,
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    let Some(dev) = device.as_mut() else { return };
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
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    if cancel.is_cancelled() {
        *job = None;
    } else if let Err(error) = next.validate() {
        fail(error, "Run failed", device, job, emit);
    } else if device.is_none() {
        fail(
            kcsdi_core::Error::NotConnected,
            "Run failed",
            device,
            job,
            emit,
        );
    } else {
        *job = Some(SweepRequest {
            request_id,
            cancel,
            plan: next,
            next_group: 0,
        });
    }
}

fn fail<T: kcsdi_core::transport::Transport>(
    error: kcsdi_core::Error,
    context: &str,
    device: &mut Option<Device<T>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    *job = None;
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
        SweepRequest {
            request_id: 0,
            cancel: CancellationToken::default(),
            plan: plan(settings),
            next_group: 0,
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
            SweepPlan { groups: Vec::new() },
            SweepPlan {
                groups: vec![
                    valid.groups[0].clone(),
                    AcquisitionGroup {
                        settings: AcquisitionSettings::Spec(invalid_params),
                        members: vec![TraceId(2)],
                    },
                ],
            },
            SweepPlan {
                groups: vec![valid.groups[0].clone(), valid.groups[0].clone()],
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
            let mut device = Some(Device::new(transport));
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
                        host: "127.0.0.1".into(),
                        port,
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
                    host: "127.0.0.1".into(),
                    port: 0,
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
            TcpTransport::connect("127.0.0.1", port).unwrap(),
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
                TcpTransport::connect("127.0.0.1", port).unwrap(),
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
                assert_eq!(peer.read(&mut byte).unwrap(), 0);
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
                        host: "127.0.0.1".into(),
                        port,
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
            let mut device: Option<Device<TcpTransport>> = None;
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
            TcpTransport::connect("127.0.0.1", port).unwrap(),
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
                if self.sends == 7 {
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
