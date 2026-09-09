// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Source ownership checks against an isolated loopback peer.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

use kcsdi_core::source::{
    Modulation, SourceAmplitude, SourceKind, SourceParams, SourcePort, SourceWarning,
};

const WAIT: Duration = Duration::from_secs(10);
const IDENTITY: &[u8] = b"$start,device\n$Synthetic peer\n$<-User @ :replay>\n$<-Software ver:test>\n$<-Hardware ver:test>\n$<-Serial num:000000000001>\n$<-Copyright:Test fixture>\n$end\n";
const COMPLETE: &[u8] =
    b"$start,s11,z\n$1000000,50,50,0\n$1500000,50,50,0\n$2000000,50,50,0\n$end\n";
const PREFIX: &[u8] = b"$start,s11,z\n$1000000,50,50,0\n";
const S11_RUN: &str = "$s11,run,caloff,z,2,ss,1000000,2000000\n";

struct Replay {
    commands: mpsc::Sender<CommandEnvelope>,
    events: mpsc::Receiver<EventEnvelope>,
    event_sender: mpsc::SyncSender<EventEnvelope>,
    wire: mpsc::Receiver<String>,
    inject: mpsc::Sender<(Vec<u8>, mpsc::Sender<()>)>,
    shutdown: CancellationToken,
    requests: Vec<CancellationToken>,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl Replay {
    fn new(reply: &'static [u8], event_capacity: usize, recording: RecordingState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (wire_tx, wire) = mpsc::channel();
        let (inject, injected) = mpsc::channel();
        let server = thread::spawn(move || {
            let started = Instant::now();
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < WAIT, "loopback accept timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            serve(socket, wire_tx, injected, reply);
        });
        let (commands, command_rx) = mpsc::channel();
        let (event_sender, events) = mpsc::sync_channel(event_capacity);
        let shutdown = CancellationToken::default();
        let worker_shutdown = shutdown.clone();
        let sender = event_sender.clone();
        let worker = thread::spawn(move || {
            run_worker(
                command_rx,
                sender,
                egui::Context::default(),
                worker_shutdown,
                PreviewMailbox::default(),
                recording,
            );
        });
        let mut replay = Self {
            commands,
            events,
            event_sender,
            wire,
            inject,
            shutdown,
            requests: Vec::new(),
            worker: Some(worker),
            server: Some(server),
        };
        replay.send(
            1,
            WorkerCommand::Connect {
                host: "127.0.0.1".into(),
                port,
            },
        );
        assert!(matches!(replay.event().event, WorkerEvent::Connected(_)));
        replay.expect_wire(&["C", "$device\n"]);
        replay
    }

    fn send(&mut self, request_id: u64, command: WorkerCommand) -> CancellationToken {
        let cancel = CancellationToken::default();
        self.requests.push(cancel.clone());
        self.commands
            .send(CommandEnvelope {
                session_id: 1,
                request_id,
                cancel: cancel.clone(),
                command,
            })
            .unwrap();
        cancel
    }

    fn event(&self) -> EventEnvelope {
        self.events
            .recv_timeout(WAIT)
            .expect("worker event timed out")
    }

    fn expect_report(
        &self,
        request_id: u64,
        state: SourceOutputState,
        warning: Option<SourceWarning>,
    ) {
        let event = self.event();
        assert_eq!((event.session_id, event.request_id), (1, request_id));
        assert!(matches!(event.event, WorkerEvent::SourceReport(report)
            if report.state == state && report.warning == warning));
    }

    fn expect_wire(&self, expected: &[&str]) {
        for &command in expected {
            assert_eq!(self.wire.recv_timeout(WAIT).unwrap(), command);
        }
    }

    fn quiet_wire(&self) {
        assert!(matches!(
            self.wire.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
    }

    fn inject(&self, bytes: &[u8]) {
        let (sent, received) = mpsc::channel();
        self.inject.send((bytes.to_vec(), sent)).unwrap();
        received.recv_timeout(WAIT).unwrap();
    }

    fn first_source(&mut self, request_id: u64) -> CancellationToken {
        let cancel = self.send(
            request_id,
            WorkerCommand::StartSource(source(SourceKind::Rf)),
        );
        self.expect_report(
            request_id,
            SourceOutputState::Requested(SourceKind::Rf),
            None,
        );
        self.expect_wire(&[
            "$s11,stop\n",
            "$s21,stop\n",
            "$spec,stop\n",
            "$rfsource,stop\n",
            "$afsource,stop\n",
            "$device\n",
            "$rfsource,init\n",
            "$device\n",
            "$rfsource,run,off,port1,1000000000,-20,0,0\n",
        ]);
        cancel
    }

    fn disconnect(&mut self, request_id: u64) {
        self.commands
            .send(CommandEnvelope {
                session_id: 2,
                request_id,
                cancel: CancellationToken::default(),
                command: WorkerCommand::Disconnect,
            })
            .unwrap();
        let event = self.event();
        assert_eq!(event.session_id, 2);
        assert!(matches!(event.event, WorkerEvent::Disconnected));
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        for cancel in &self.requests {
            cancel.cancel();
        }
        self.shutdown.cancel();
        let _ = self.commands.send(CommandEnvelope {
            session_id: 2,
            request_id: u64::MAX,
            cancel: CancellationToken::default(),
            command: WorkerCommand::Shutdown,
        });
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
        if let Some(server) = self.server.take() {
            let result = server.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn serve(
    mut socket: TcpStream,
    wire: mpsc::Sender<String>,
    injected: mpsc::Receiver<(Vec<u8>, mpsc::Sender<()>)>,
    sweep_reply: &[u8],
) {
    socket.set_nodelay(true).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut line = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "loopback fixture exceeded its deadline"
        );
        while let Ok((bytes, sent)) = injected.try_recv() {
            socket.write_all(&bytes).unwrap();
            let _ = sent.send(());
        }
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("loopback read failed: {error}"),
        }
        let command = if line.is_empty() && matches!(byte[0], b'C' | 3) {
            String::from_utf8(vec![byte[0]]).unwrap()
        } else {
            line.push(byte[0]);
            assert!(line.len() < 4_096);
            if byte[0] != b'\n' {
                continue;
            }
            String::from_utf8(std::mem::take(&mut line)).unwrap()
        };
        let response = match command.as_str() {
            "C" => b"$start,id\n$000000000001\n$end\n".as_slice(),
            "$device\n" => IDENTITY,
            S11_RUN => sweep_reply,
            "$temp\n" => b"$start,temp\n$42\n$end\n",
            "$voltage\n" => b"$start,voltage\n$12,8\n$end\n",
            _ => b"",
        };
        wire.send(command).unwrap();
        socket.write_all(response).unwrap();
    }
}

fn source(kind: SourceKind) -> SourceParams {
    SourceParams {
        kind,
        port: SourcePort::Port1,
        frequency_hz: 1_000_000_000,
        amplitude: SourceAmplitude::Dbm(-20),
        modulation: Modulation::Off,
    }
}

fn plan() -> SweepPlan {
    let mut plan = SweepPlan::from_requests([(
        TraceId(1),
        AcquisitionSettings::S11(crate::acquisition::tests::s11()),
    )])
    .unwrap();
    plan.run.interval_ms = 60_000;
    plan
}

#[test]
fn rf_af_late_warnings_status_exclusion_and_explicit_stop_before_scan() {
    let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
    let rf_cancel = replay.first_source(2);
    replay.inject(b"$start,warn_gtr\n$fixture");
    thread::sleep(Duration::from_millis(100));
    replay.inject(b" warning\n$end\n");
    replay.expect_report(
        2,
        SourceOutputState::Requested(SourceKind::Rf),
        Some(SourceWarning::AboveMaximum),
    );
    replay.inject(b"$start,warn_lt\n$fixture warning\n$end\n");
    replay.expect_report(
        2,
        SourceOutputState::Requested(SourceKind::Rf),
        Some(SourceWarning::BelowMinimum),
    );

    replay.send(1, WorkerCommand::RefreshStatus);
    assert!(matches!(replay.event().event, WorkerEvent::StatusFailed(_)));
    replay.send(3, WorkerCommand::RunWorkspace(plan()));
    assert!(matches!(replay.event().event, WorkerEvent::Error(message)
        if message.contains("stop the source")));
    replay.quiet_wire();

    rf_cancel.cancel();
    let af_cancel = replay.send(4, WorkerCommand::StartSource(source(SourceKind::Af)));
    replay.expect_report(4, SourceOutputState::Requested(SourceKind::Af), None);
    replay.expect_wire(&[
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$rfsource,stop\n",
        "$device\n",
        "$afsource,init\n",
        "$device\n",
        "$afsource,run,off,port1,1000000000,-20,0,0\n",
    ]);
    af_cancel.cancel();
    replay.send(5, WorkerCommand::StopSource);
    replay.expect_report(5, SourceOutputState::StopSent, None);
    replay.expect_wire(&["$afsource,stop\n", "$device\n"]);
    replay.send(6, WorkerCommand::RunWorkspace(plan()));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::RunProgress(RunProgress::Acquiring)
    ));
    assert!(matches!(replay.event().event, WorkerEvent::SweepTrace(_)));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::RunProgress(RunProgress::Waiting { .. })
    ));
    replay.expect_wire(&[
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$s11,init\n",
        "$bw,10k\n",
        S11_RUN,
    ]);
    replay.disconnect(7);
    replay.expect_wire(&["$s11,stop\n", "$local\n"]);
}

#[test]
fn late_source_error_releases_the_device_and_reports_unknown() {
    let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
    replay.first_source(2);
    replay.inject(b"$start,err_par5\n$fixture error\n$end\n");
    replay.expect_wire(&["$rfsource,stop\n", "$local\n"]);
    replay.expect_report(2, SourceOutputState::Unknown, None);
    assert!(
        matches!(replay.event().event, WorkerEvent::ConnectionLost(message)
        if message.contains("err_par5"))
    );
    replay.send(3, WorkerCommand::RunWorkspace(plan()));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::ConnectionLost(_)
    ));
}

#[test]
fn late_source_error_releases_output_before_terminal_event_backpressure() {
    let mut replay = Replay::new(COMPLETE, 1, RecordingState::default());
    replay.first_source(2);
    replay
        .event_sender
        .send(
            WorkerIdentity {
                session_id: 1,
                request_id: 1,
            }
            .event(WorkerEvent::SweepStopped),
        )
        .unwrap();
    replay.inject(b"$start,err_par5\n$fixture error\n$end\n");
    replay.expect_wire(&["$rfsource,stop\n", "$local\n"]);
    assert!(matches!(replay.event().event, WorkerEvent::SweepStopped));
    replay.expect_report(2, SourceOutputState::Unknown, None);
    assert!(matches!(
        replay.event().event,
        WorkerEvent::ConnectionLost(_)
    ));
}

#[test]
fn disconnect_releases_an_active_source_without_starting_a_replacement() {
    let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
    replay.first_source(2);
    replay.disconnect(3);
    replay.expect_wire(&["$rfsource,stop\n", "$local\n"]);
}

#[test]
fn full_event_queue_cannot_delay_source_stop_after_late_warning() {
    let mut replay = Replay::new(COMPLETE, 1, RecordingState::default());
    let cancel = replay.first_source(2);
    replay
        .event_sender
        .send(
            WorkerIdentity {
                session_id: 1,
                request_id: 1,
            }
            .event(WorkerEvent::SweepStopped),
        )
        .unwrap();
    replay.inject(b"$start,warn_gtr\n$fixture warning\n$end\n");
    thread::sleep(Duration::from_millis(200));
    cancel.cancel();
    replay.send(3, WorkerCommand::StopSource);
    // The receiver is deliberately left full until source cleanup is observed.
    replay.expect_wire(&["$rfsource,stop\n", "$device\n"]);
    assert!(matches!(replay.event().event, WorkerEvent::SweepStopped));
    replay.expect_report(
        3,
        SourceOutputState::StopSent,
        Some(SourceWarning::AboveMaximum),
    );
    replay.disconnect(4);
    replay.expect_wire(&["$local\n"]);
}

#[test]
fn rejected_status_and_scan_requests_do_not_block_stop_behind_a_full_queue() {
    for status in [true, false] {
        let mut replay = Replay::new(COMPLETE, 1, RecordingState::default());
        let cancel = replay.first_source(2);
        replay
            .event_sender
            .send(
                WorkerIdentity {
                    session_id: 1,
                    request_id: 1,
                }
                .event(WorkerEvent::SweepStopped),
            )
            .unwrap();
        if status {
            replay.send(1, WorkerCommand::RefreshStatus);
        } else {
            replay.send(3, WorkerCommand::RunWorkspace(plan()));
        }
        thread::sleep(Duration::from_millis(200));
        cancel.cancel();
        replay.send(4, WorkerCommand::StopSource);
        replay.expect_wire(&["$rfsource,stop\n", "$device\n"]);
        assert!(matches!(replay.event().event, WorkerEvent::SweepStopped));
        replay.expect_report(4, SourceOutputState::StopSent, None);
        replay.disconnect(5);
        replay.expect_wire(&["$local\n"]);
    }
}

#[test]
fn invalid_source_start_keeps_the_existing_warning_owner() {
    let identity = WorkerIdentity {
        session_id: 1,
        request_id: 2,
    };
    let original_cancel = CancellationToken::default();
    let mut owner = Some((identity, original_cancel.clone()));
    let invalid = SourceParams {
        port: SourcePort::AfOut,
        ..source(SourceKind::Rf)
    };
    track_source_request(
        &CommandEnvelope {
            session_id: 1,
            request_id: 3,
            cancel: CancellationToken::default(),
            command: WorkerCommand::StartSource(invalid),
        },
        identity,
        &mut owner,
    );
    let (owner, token) = owner.unwrap();
    assert_eq!((owner.session_id, owner.request_id), (1, 2));
    original_cancel.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn source_start_cancels_an_incomplete_recording_pass() {
    let directory = tempfile::tempdir().unwrap();
    let mut plan = plan();
    plan.run.recording.enabled = true;
    plan.run.recording.directory = directory.path().to_owned();
    let mut replay = Replay::new(PREFIX, EVENT_CAPACITY, RecordingState::default());
    let sweep_cancel = replay.send(2, WorkerCommand::RunWorkspace(plan));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::RunProgress(RunProgress::Acquiring)
    ));
    replay.expect_wire(&[
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$s11,init\n",
        "$bw,10k\n",
        S11_RUN,
    ]);
    // The UI cancels the acquisition token before queuing a source request.
    sweep_cancel.cancel();
    let source_cancel = replay.send(3, WorkerCommand::StartSource(source(SourceKind::Rf)));
    replay.expect_report(3, SourceOutputState::Requested(SourceKind::Rf), None);
    replay.expect_wire(&[
        "\x03",
        "$device\n",
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$rfsource,stop\n",
        "$afsource,stop\n",
        "$device\n",
        "$rfsource,init\n",
        "$device\n",
        "$rfsource,run,off,port1,1000000000,-20,0,0\n",
    ]);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    source_cancel.cancel();
    replay.send(4, WorkerCommand::StopSource);
    replay.expect_report(4, SourceOutputState::StopSent, None);
    replay.expect_wire(&["$rfsource,stop\n", "$device\n"]);
    replay.disconnect(5);
    replay.expect_wire(&["$local\n"]);
}

#[test]
fn pending_recording_writer_does_not_block_source_start_or_stop() {
    let directory = tempfile::tempdir().unwrap();
    let mut plan = plan();
    plan.run.recording.enabled = true;
    plan.run.recording.directory = directory.path().to_owned();
    let (writing, entered) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let writer = RecordWriter::with_test_save(move |key| {
        writing.send(key).unwrap();
        wait_release
            .recv_timeout(WAIT)
            .map_err(|error| error.to_string())?;
        Ok(None)
    })
    .unwrap();
    let mut replay = Replay::new(
        COMPLETE,
        EVENT_CAPACITY,
        RecordingState {
            writer: Some(writer),
            pending: None,
        },
    );
    let sweep_cancel = replay.send(2, WorkerCommand::RunWorkspace(plan));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::RunProgress(RunProgress::Acquiring)
    ));
    assert!(matches!(replay.event().event, WorkerEvent::SweepTrace(_)));
    assert!(matches!(
        replay.event().event,
        WorkerEvent::RunProgress(RunProgress::Saving)
    ));
    assert_eq!(entered.recv_timeout(WAIT).unwrap().request_id, 2);
    replay.expect_wire(&[
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$s11,init\n",
        "$bw,10k\n",
        S11_RUN,
    ]);
    let source_cancel = replay.send(3, WorkerCommand::StartSource(source(SourceKind::Rf)));
    replay.expect_report(3, SourceOutputState::Requested(SourceKind::Rf), None);
    assert!(sweep_cancel.is_cancelled());
    replay.expect_wire(&[
        "$s11,stop\n",
        "$s11,stop\n",
        "$s21,stop\n",
        "$spec,stop\n",
        "$rfsource,stop\n",
        "$afsource,stop\n",
        "$device\n",
        "$rfsource,init\n",
        "$device\n",
        "$rfsource,run,off,port1,1000000000,-20,0,0\n",
    ]);
    source_cancel.cancel();
    replay.send(4, WorkerCommand::StopSource);
    replay.expect_report(4, SourceOutputState::StopSent, None);
    replay.expect_wire(&["$rfsource,stop\n", "$device\n"]);
    release.send(()).unwrap();
    replay.quiet_wire();
    assert!(replay.events.try_recv().is_err());
    replay.disconnect(5);
    replay.expect_wire(&["$local\n"]);
}
