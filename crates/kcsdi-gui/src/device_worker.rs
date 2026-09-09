// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Device worker thread: owns the instrument connection.
//!
//! Mirrors the ptouch-rs worker pattern. All blocking I/O happens here;
//! the UI sends [`WorkerCommand`]s and receives [`WorkerEvent`]s over
//! two mpsc channels. After every event the worker calls
//! `ctx.request_repaint()` so the UI picks it up immediately.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use kcsdi_core::Device;
use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::SweepData;
use kcsdi_core::device::{S11Params, SpecParams};
use kcsdi_core::transport::TcpTransport;
use log::{error, info};

use crate::preview::{PreviewEnvelope, PreviewMailbox};
use crate::state::{CommandEnvelope, DEVICE_MODEL, EventEnvelope, WorkerCommand, WorkerEvent};

pub const EVENT_CAPACITY: usize = 16;

/// A repeating sweep job requested by the UI.
#[derive(Debug, Clone)]
enum SweepJob {
    Spec(SpecParams),
    S11(S11Params),
}

#[derive(Debug, Clone)]
struct SweepRequest {
    request_id: u64,
    cancel: CancellationToken,
    job: SweepJob,
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

    let emit = |evt: EventEnvelope| {
        send_event(&evt_tx, evt, &shutdown, None, &ctx);
    };

    'worker: loop {
        if shutdown.is_cancelled() {
            break;
        }
        if device.is_some() && job.is_some() {
            // Run one sweep, then drain any pending commands.
            let current = job.clone().expect("checked above");
            if current.cancel.is_cancelled() {
                job = None;
                continue;
            }
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
                    expected_points: progress.expected_points,
                });
                ctx.request_repaint();
            };
            let dev = device.as_mut().expect("checked above");
            let result = match &current.job {
                SweepJob::Spec(params) => {
                    dev.sweep_spec_controlled(params, &current.cancel, progress)
                }
                SweepJob::S11(params) => {
                    dev.sweep_s11_controlled(params, &current.cancel, progress)
                }
            };
            match result {
                Ok(data) => {
                    let mut event = source.event(WorkerEvent::SweepTrace(data));
                    event.cycle_id = Some(cycle_id);
                    send_event(&evt_tx, event, &shutdown, Some(&current.cancel), &ctx);
                }
                Err(e) => {
                    if !matches!(e, kcsdi_core::Error::Cancelled) {
                        error!("sweep failed: {e}");
                    }
                    fail(e, "Sweep failed", &mut device, &mut job, &emit_result);
                }
            }
            loop {
                if shutdown.is_cancelled() {
                    break 'worker;
                }
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if matches!(cmd.command, WorkerCommand::Shutdown) {
                            break 'worker;
                        }
                        handle(cmd, &mut identity, &mut device, &mut job, &emit);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break 'worker,
                }
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if shutdown.is_cancelled() || matches!(cmd.command, WorkerCommand::Shutdown) {
                        break;
                    }
                    handle(cmd, &mut identity, &mut device, &mut job, &emit);
                }
                Err(_) => break,
            }
        }
    }
    shutdown.cancel();
    if let Some(mut dev) = device.take() {
        dev.close();
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
        if session_id != identity.session_id || request_id < identity.request_id {
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
        WorkerCommand::RunSpec(params) => {
            start_sweep(
                SweepJob::Spec(params),
                request_id,
                cancel,
                device,
                job,
                &emit,
            );
        }
        WorkerCommand::RunS11(params) => {
            start_sweep(
                SweepJob::S11(params),
                request_id,
                cancel,
                device,
                job,
                &emit,
            );
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
            if let Some(dev) = device.as_mut() {
                match dev
                    .temperature_controlled(&cancel)
                    .and_then(|t| dev.voltage_controlled(&cancel).map(|v| (t, v)))
                {
                    Ok((temperature, voltage)) => {
                        emit(WorkerEvent::Status {
                            temperature,
                            voltage,
                        });
                    }
                    Err(e) => fail(e, "Status query failed", device, job, &emit),
                }
            }
        }
    }
}

fn start_sweep(
    next: SweepJob,
    request_id: u64,
    cancel: CancellationToken,
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    if cancel.is_cancelled() {
        *job = None;
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
            job: next,
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
    use kcsdi_core::Error;
    use std::cell::RefCell;

    fn request(job: SweepJob) -> SweepRequest {
        SweepRequest {
            request_id: 0,
            cancel: CancellationToken::default(),
            job,
        }
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
                command: WorkerCommand::RunS11(
                    crate::state::S11State::default().s11_params().unwrap(),
                ),
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
                WorkerCommand::RunS11(crate::state::S11State::default().s11_params().unwrap()),
            ),
            (
                3,
                10,
                WorkerCommand::RunSpec(crate::state::SpecState::default().spec_params().unwrap()),
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
            WorkerCommand::RunS11(crate::state::S11State::default().s11_params().unwrap()),
            WorkerCommand::RunSpec(crate::state::SpecState::default().spec_params().unwrap()),
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
            job: SweepJob::S11(crate::state::S11State::default().s11_params().unwrap()),
        });
        let events = RefCell::new(Vec::new());
        handle(
            CommandEnvelope {
                session_id: 2,
                request_id: 7,
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
        assert_eq!(job.unwrap().request_id, 7);
        assert!(device.is_some());
        let events = events.borrow();
        assert_eq!((events[0].session_id, events[0].request_id), (2, 7));
        assert!(matches!(
            events[0].event,
            WorkerEvent::Status {
                temperature: 42.0,
                ..
            }
        ));
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
            let mut job = Some(request(SweepJob::S11(
                crate::state::S11State::default().s11_params().unwrap(),
            )));
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
        let mut job = Some(request(SweepJob::S11(
            crate::state::S11State::default().s11_params().unwrap(),
        )));
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
                if self.sends == 5 {
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
        let params = crate::state::S11State::default().s11_params().unwrap();
        let error = device.as_mut().unwrap().sweep_s11(&params).unwrap_err();
        assert!(matches!(&error, Error::Device(name) if name == "err_par5"));
        let mut job = Some(request(SweepJob::S11(params)));
        let events = RefCell::new(Vec::new());
        fail(error, "Sweep failed", &mut device, &mut job, &|event| {
            events.borrow_mut().push(event);
        });
        assert!(device.is_none());
        assert!(job.is_none());
        assert!(matches!(events.borrow()[0], WorkerEvent::ConnectionLost(_)));
    }
}
