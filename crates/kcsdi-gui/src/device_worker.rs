// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Device worker thread: owns the instrument connection.
//!
//! Mirrors the ptouch-rs worker pattern. All blocking I/O happens here;
//! the UI sends [`WorkerCommand`]s and receives [`WorkerEvent`]s over
//! two mpsc channels. After every event the worker calls
//! `ctx.request_repaint()` so the UI picks it up immediately.

use std::sync::mpsc;

use kcsdi_core::Device;
use kcsdi_core::device::{S11Params, SpecParams};
use kcsdi_core::transport::TcpTransport;
use log::{error, info};

use crate::state::{CommandEnvelope, DEVICE_MODEL, EventEnvelope, WorkerCommand, WorkerEvent};

/// A repeating sweep job requested by the UI.
#[derive(Debug, Clone)]
enum SweepJob {
    Spec(SpecParams),
    S11(S11Params),
}

#[derive(Debug, Clone)]
struct SweepRequest {
    request_id: u64,
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
            event,
        }
    }
}

/// Entry point of the `"device-worker"` thread.
pub fn device_worker(
    cmd_rx: mpsc::Receiver<CommandEnvelope>,
    evt_tx: mpsc::Sender<EventEnvelope>,
    ctx: egui::Context,
) {
    let mut device: Option<Device<TcpTransport>> = None;
    let mut job: Option<SweepRequest> = None;
    let mut identity = WorkerIdentity::default();

    let emit = |evt: EventEnvelope| {
        if evt_tx.send(evt).is_err() {
            info!("UI went away, worker exiting");
        }
        ctx.request_repaint();
    };

    loop {
        if device.is_some() && job.is_some() {
            // Run one sweep, then drain any pending commands.
            let current = job.clone().expect("checked above");
            let source = WorkerIdentity {
                session_id: identity.session_id,
                request_id: current.request_id,
            };
            let emit_result = |event| emit(source.event(event));
            let dev = device.as_mut().expect("checked above");
            let result = match &current.job {
                SweepJob::Spec(params) => dev.sweep_spec(params),
                SweepJob::S11(params) => dev.sweep_s11(params),
            };
            match result {
                Ok(data) => emit_result(WorkerEvent::SweepTrace(data)),
                Err(e) => {
                    error!("sweep failed: {e}");
                    fail(e, "Sweep failed", &mut device, &mut job, &emit_result);
                }
            }
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut identity, &mut device, &mut job, &emit),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => handle(cmd, &mut identity, &mut device, &mut job, &emit),
                Err(_) => return,
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
        command,
    } = envelope;
    if matches!(
        command,
        WorkerCommand::Connect { .. } | WorkerCommand::Disconnect
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
            match Device::connect_with_model(&host, port, DEVICE_MODEL).and_then(|mut dev| {
                let info = dev.device_info()?;
                Ok((dev, info))
            }) {
                Ok((dev, info)) => {
                    info!("connected to {host}:{port}, serial {}", info.serial);
                    *device = Some(dev);
                    emit(WorkerEvent::Connected(info));
                }
                Err(e) => {
                    error!("connect to {host}:{port} failed: {e}");
                    *device = None;
                    emit(WorkerEvent::Error(format!("Connect failed: {e}")));
                }
            }
        }
        WorkerCommand::Disconnect => {
            *job = None;
            if let Some(mut dev) = device.take() {
                dev.close();
            }
            emit(WorkerEvent::Disconnected);
        }
        WorkerCommand::RunSpec(params) => {
            start_sweep(SweepJob::Spec(params), request_id, device, job, &emit);
        }
        WorkerCommand::RunS11(params) => {
            start_sweep(SweepJob::S11(params), request_id, device, job, &emit);
        }
        WorkerCommand::StopSweep => {
            *job = None;
            if let Some(dev) = device.as_mut()
                && let Err(e) = dev.stop_sweep()
            {
                fail(e, "Stop failed", device, job, &emit);
            }
        }
        WorkerCommand::RefreshStatus => {
            if let Some(dev) = device.as_mut() {
                match dev
                    .temperature()
                    .and_then(|t| dev.voltage().map(|v| (t, v)))
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
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepRequest>,
    emit: &dyn Fn(WorkerEvent),
) {
    if device.is_none() {
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
    } else {
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
        SweepRequest { request_id: 0, job }
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
            job: SweepJob::S11(crate::state::S11State::default().s11_params().unwrap()),
        });
        let events = RefCell::new(Vec::new());
        handle(
            CommandEnvelope {
                session_id: 2,
                request_id: 7,
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
            fn send(&mut self, _: &[u8]) -> kcsdi_core::Result<()> {
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
