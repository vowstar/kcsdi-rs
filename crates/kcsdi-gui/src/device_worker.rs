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
use kcsdi_core::device::SpecParams;
use kcsdi_core::transport::TcpTransport;
use log::{error, info};

use crate::state::{WorkerCommand, WorkerEvent};

/// Entry point of the `"device-worker"` thread.
pub fn device_worker(
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    evt_tx: mpsc::Sender<WorkerEvent>,
    ctx: egui::Context,
) {
    let mut device: Option<Device<TcpTransport>> = None;
    let mut spec: Option<SpecParams> = None;

    let emit = |evt: WorkerEvent| {
        if evt_tx.send(evt).is_err() {
            info!("UI went away, worker exiting");
        }
        ctx.request_repaint();
    };

    loop {
        if device.is_some() && spec.is_some() {
            // Run one sweep, then drain any pending commands.
            let params = spec.clone().expect("checked above");
            let dev = device.as_mut().expect("checked above");
            match dev.sweep_spec(&params) {
                Ok(data) => emit(WorkerEvent::SpecTrace(data)),
                Err(e) => {
                    error!("sweep failed: {e}");
                    spec = None;
                    emit(WorkerEvent::Error(format!("Sweep failed: {e}")));
                }
            }
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut device, &mut spec, &emit),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => handle(cmd, &mut device, &mut spec, &emit),
                Err(_) => return,
            }
        }
    }
}

/// Apply one command to the worker-local connection state.
fn handle(
    cmd: WorkerCommand,
    device: &mut Option<Device<TcpTransport>>,
    spec: &mut Option<SpecParams>,
    emit: &dyn Fn(WorkerEvent),
) {
    match cmd {
        WorkerCommand::Connect { host, port } => {
            *spec = None;
            match Device::connect(&host, port).and_then(|mut dev| {
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
            *spec = None;
            if let Some(mut dev) = device.take() {
                dev.close();
            }
            emit(WorkerEvent::Disconnected);
        }
        WorkerCommand::RunSpec(params) => {
            *spec = Some(params);
        }
        WorkerCommand::StopSpec => {
            *spec = None;
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
                    Err(e) => emit(WorkerEvent::Error(format!("Status query failed: {e}"))),
                }
            }
        }
    }
}
