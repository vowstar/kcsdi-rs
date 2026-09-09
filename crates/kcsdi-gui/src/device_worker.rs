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

use crate::state::{DEVICE_MODEL, WorkerCommand, WorkerEvent};

/// A repeating sweep job requested by the UI.
#[derive(Debug, Clone)]
enum SweepJob {
    Spec(SpecParams),
    S11(S11Params),
}

/// Entry point of the `"device-worker"` thread.
pub fn device_worker(
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    evt_tx: mpsc::Sender<WorkerEvent>,
    ctx: egui::Context,
) {
    let mut device: Option<Device<TcpTransport>> = None;
    let mut job: Option<SweepJob> = None;

    let emit = |evt: WorkerEvent| {
        if evt_tx.send(evt).is_err() {
            info!("UI went away, worker exiting");
        }
        ctx.request_repaint();
    };

    loop {
        if device.is_some() && job.is_some() {
            // Run one sweep, then drain any pending commands.
            let current = job.clone().expect("checked above");
            let dev = device.as_mut().expect("checked above");
            let result = match &current {
                SweepJob::Spec(params) => dev.sweep_spec(params),
                SweepJob::S11(params) => dev.sweep_s11(params),
            };
            match result {
                Ok(data) => emit(WorkerEvent::SweepTrace(data)),
                Err(e) => {
                    error!("sweep failed: {e}");
                    fail(e, "Sweep failed", &mut device, &mut job, &emit);
                }
            }
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut device, &mut job, &emit),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => handle(cmd, &mut device, &mut job, &emit),
                Err(_) => return,
            }
        }
    }
}

/// Apply one command to the worker-local connection state.
fn handle(
    cmd: WorkerCommand,
    device: &mut Option<Device<TcpTransport>>,
    job: &mut Option<SweepJob>,
    emit: &dyn Fn(WorkerEvent),
) {
    match cmd {
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
            *job = Some(SweepJob::Spec(params));
        }
        WorkerCommand::RunS11(params) => {
            *job = Some(SweepJob::S11(params));
        }
        WorkerCommand::StopSweep => {
            *job = None;
            if let Some(dev) = device.as_mut()
                && let Err(e) = dev.stop_sweep()
            {
                fail(e, "Stop failed", device, job, emit);
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
                    Err(e) => fail(e, "Status query failed", device, job, emit),
                }
            }
        }
    }
}

fn fail<T: kcsdi_core::transport::Transport>(
    error: kcsdi_core::Error,
    context: &str,
    device: &mut Option<Device<T>>,
    job: &mut Option<SweepJob>,
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

    #[test]
    fn fatal_failures_clear_the_job_and_report_connection_loss() {
        for error in [
            Error::Timeout,
            Error::NotConnected,
            Error::Protocol("oversized packet".into()),
            Error::Io(std::io::ErrorKind::ConnectionReset.into()),
        ] {
            let mut device: Option<Device<TcpTransport>> = None;
            let mut job = Some(SweepJob::S11(
                crate::state::S11State::default().s11_params().unwrap(),
            ));
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
        let mut job = Some(SweepJob::S11(
            crate::state::S11State::default().s11_params().unwrap(),
        ));
        let events = RefCell::new(Vec::new());
        handle(
            WorkerCommand::RefreshStatus,
            &mut device,
            &mut job,
            &|event| {
                events.borrow_mut().push(event);
            },
        );
        server.join().unwrap();
        assert!(device.is_none());
        assert!(job.is_none());
        assert!(matches!(events.borrow()[0], WorkerEvent::ConnectionLost(_)));
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
        let mut job = Some(SweepJob::S11(params));
        let events = RefCell::new(Vec::new());
        fail(error, "Sweep failed", &mut device, &mut job, &|event| {
            events.borrow_mut().push(event);
        });
        assert!(device.is_none());
        assert!(job.is_none());
        assert!(matches!(events.borrow()[0], WorkerEvent::ConnectionLost(_)));
    }
}
