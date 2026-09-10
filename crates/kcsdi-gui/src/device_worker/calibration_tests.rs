// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Calibration ownership against an isolated TCP peer, without instrument I/O.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

use kcsdi_core::calibration::{
    CalibrationKind, CalibrationParams, CalibrationPrompt, UserCalibrationParams,
};
use kcsdi_core::model::Rbw;
use kcsdi_core::source::{Modulation, SourceAmplitude, SourceKind, SourceParams, SourcePort};

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
    tokens: Vec<CancellationToken>,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl Replay {
    fn new(reply: &'static [u8], capacity: usize, recording: RecordingState) -> Self {
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
                        assert!(started.elapsed() < WAIT, "fixture accept timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            serve(socket, wire_tx, injected, reply);
        });
        let (commands, command_rx) = mpsc::channel();
        let (event_sender, events) = mpsc::sync_channel(capacity);
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
            tokens: Vec::new(),
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
        self.tokens.push(cancel.clone());
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

    fn expect_report(&self, request_id: u64, kind: CalibrationKind, phase: CalibrationPhase) {
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < WAIT,
                "expected calibration phase {phase:?}"
            );
            let event = self.event();
            assert_eq!((event.session_id, event.request_id), (1, request_id));
            let WorkerEvent::CalibrationReport(report) = event.event else {
                panic!("unexpected worker event: {:?}", event.event);
            };
            assert_eq!(report.kind, Some(kind));
            if report.phase == phase {
                return;
            }
            assert!(
                matches!(
                    report.phase,
                    CalibrationPhase::AwaitingConfirmation
                        | CalibrationPhase::WarmingUp
                        | CalibrationPhase::Measuring
                        | CalibrationPhase::Processing
                        | CalibrationPhase::Saving
                ),
                "unexpected calibration phase: {:?}",
                report.phase
            );
        }
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

    fn packet(&self, name: &str) {
        self.inject(format!("$start,{name}\n$fixture\n$end\n").as_bytes());
    }

    fn start(&mut self, request_id: u64, params: CalibrationParams) -> CancellationToken {
        let token = self.send(request_id, WorkerCommand::StartCalibration(params));
        self.expect_setup(params);
        self.expect_report(
            request_id,
            params.kind(),
            CalibrationPhase::AwaitingConfirmation,
        );
        self.packet(&format!("{}_confirm", prefix(params.kind())));
        self.expect_wire(&["$yes\n"]);
        let prompt = first_prompt(params.kind());
        self.packet(prompt_name(params.kind(), prompt));
        self.expect_report(request_id, params.kind(), CalibrationPhase::Prompt(prompt));
        token
    }

    fn expect_setup(&self, params: CalibrationParams) {
        let (rbw, command) = match params {
            CalibrationParams::S11System => ("$bw,1k\n", "$cal_s11\n"),
            CalibrationParams::S21System => ("$bw,1k\n", "$cal_s21\n"),
            CalibrationParams::S11User(_) => ("$bw,3k\n", "$cal_user_s11,1500000,1000001,200\n"),
            CalibrationParams::S21User(_) => ("$bw,3k\n", "$cal_user_s21,1500000,1000001,200\n"),
        };
        self.expect_wire(&[
            "$s11,stop\n",
            "$s21,stop\n",
            "$spec,stop\n",
            "$rfsource,stop\n",
            "$afsource,stop\n",
            params.kind().init_command(),
            rbw,
            command,
        ]);
    }

    fn cancel_prompt(&mut self, request_id: u64, kind: CalibrationKind, token: &CancellationToken) {
        token.cancel();
        self.send(request_id, WorkerCommand::CancelCalibration);
        self.expect_wire(&["$exit\n", kind.stop_command(), "$device\n"]);
        self.expect_report(request_id, kind, CalibrationPhase::Cancelled);
    }

    fn fill_events(&self) {
        self.event_sender
            .send(
                WorkerIdentity {
                    session_id: 1,
                    request_id: 1,
                }
                .event(WorkerEvent::SweepStopped),
            )
            .unwrap();
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        for token in &self.tokens {
            token.cancel();
        }
        self.shutdown.cancel();
        let _ = self.commands.send(CommandEnvelope {
            session_id: 2,
            request_id: u64::MAX,
            cancel: CancellationToken::default(),
            command: WorkerCommand::Shutdown,
        });
        for handle in [&mut self.worker, &mut self.server] {
            if let Some(handle) = handle.take() {
                let result = handle.join();
                if !thread::panicking() {
                    result.unwrap();
                }
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
    let mut kind = CalibrationKind::S11System;
    loop {
        assert!(Instant::now() < deadline, "fixture exceeded its deadline");
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
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return;
            }
            Err(error) => panic!("fixture read failed: {error}"),
        }
        let command = if line.is_empty() && matches!(byte[0], b'C' | 3) {
            String::from_utf8(vec![byte[0]]).unwrap()
        } else {
            line.push(byte[0]);
            assert!(line.len() < 4096);
            if byte[0] != b'\n' {
                continue;
            }
            String::from_utf8(std::mem::take(&mut line)).unwrap()
        };
        if command.starts_with("$cal_user_s11,") {
            kind = CalibrationKind::S11User;
        }
        if command.starts_with("$cal_user_s21,") {
            kind = CalibrationKind::S21User;
        }
        if command == "$cal_s11\n" {
            kind = CalibrationKind::S11System;
        }
        if command == "$cal_s21\n" {
            kind = CalibrationKind::S21System;
        }
        let exit = format!("$start,{}_exit\n$fixture\n$end\n", prefix(kind));
        let response = match command.as_str() {
            "C" => b"$start,id\n$000000000001\n$end\n".as_slice(),
            "$device\n" => IDENTITY,
            "$exit\n" => exit.as_bytes(),
            S11_RUN => sweep_reply,
            _ => b"",
        };
        wire.send(command).unwrap();
        if socket.write_all(response).is_err() {
            return;
        }
    }
}

fn prefix(kind: CalibrationKind) -> &'static str {
    match kind {
        CalibrationKind::S11System => "s11cal",
        CalibrationKind::S21System => "s21cal",
        CalibrationKind::S11User => "s11UserCal",
        CalibrationKind::S21User => "s21UserCal",
    }
}

fn first_prompt(kind: CalibrationKind) -> CalibrationPrompt {
    match kind {
        CalibrationKind::S11System | CalibrationKind::S11User => CalibrationPrompt::Short,
        CalibrationKind::S21System | CalibrationKind::S21User => CalibrationPrompt::Through,
    }
}

fn prompt_name(kind: CalibrationKind, prompt: CalibrationPrompt) -> &'static str {
    match (kind, prompt) {
        (CalibrationKind::S11System, CalibrationPrompt::Short) => "s11cal_short",
        (CalibrationKind::S11System, CalibrationPrompt::Open) => "s11cal_open",
        (CalibrationKind::S11System, CalibrationPrompt::Load) => "s11cal_load",
        (CalibrationKind::S11User, CalibrationPrompt::Short) => "s11UserCal_short",
        (CalibrationKind::S11User, CalibrationPrompt::Open) => "s11UserCal_open",
        (CalibrationKind::S11User, CalibrationPrompt::Load) => "s11Usercal_load",
        (CalibrationKind::S21System, CalibrationPrompt::Through) => "s21cal_connect",
        (CalibrationKind::S21User, CalibrationPrompt::Through) => "s21UserCal_step",
        _ => panic!("invalid fixture prompt"),
    }
}

fn params(kind: CalibrationKind) -> CalibrationParams {
    let user = UserCalibrationParams::from_range(1_000_000, 2_000_001, 201, Rbw::R3k).unwrap();
    match kind {
        CalibrationKind::S11System => CalibrationParams::S11System,
        CalibrationKind::S21System => CalibrationParams::S21System,
        CalibrationKind::S11User => CalibrationParams::S11User(user),
        CalibrationKind::S21User => CalibrationParams::S21User(user),
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
fn all_four_calibrations_require_each_standard_and_exact_complete_frame() {
    for kind in [
        CalibrationKind::S11System,
        CalibrationKind::S21System,
        CalibrationKind::S11User,
        CalibrationKind::S21User,
    ] {
        let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
        let mut token = replay.start(2, params(kind));
        let prompts: &[CalibrationPrompt] = match first_prompt(kind) {
            CalibrationPrompt::Short => &[
                CalibrationPrompt::Short,
                CalibrationPrompt::Open,
                CalibrationPrompt::Load,
            ],
            _ => &[CalibrationPrompt::Through],
        };
        let mut request = 2;
        for (index, prompt) in prompts.iter().enumerate() {
            replay.quiet_wire();
            token.cancel();
            // A Next token replacement at an idle human prompt is not Cancel.
            replay.quiet_wire();
            request += 1;
            token = replay.send(request, WorkerCommand::AdvanceCalibration(*prompt));
            replay.expect_report(request, kind, CalibrationPhase::Measuring);
            replay.expect_wire(&["$yes\n"]);
            if let Some(next) = prompts.get(index + 1) {
                replay.packet(prompt_name(kind, *next));
                replay.expect_report(request, kind, CalibrationPhase::Prompt(*next));
            }
        }
        replay.packet(&format!("{}_process", prefix(kind)));
        replay.expect_report(request, kind, CalibrationPhase::Processing);
        replay.inject(format!("$start,{}_completed\n$fixture\n", prefix(kind)).as_bytes());
        thread::sleep(Duration::from_millis(100));
        assert!(
            replay.events.try_recv().is_err(),
            "a partial completion packet must not complete"
        );
        replay.inject(b"$end\n");
        replay.expect_report(request, kind, CalibrationPhase::Completed);
        replay.quiet_wire();
        assert!(replay.events.try_recv().is_err());
    }
}

#[test]
fn stale_expected_prompt_sends_no_yes_and_does_not_retag_calibration() {
    let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
    let token = replay.start(2, CalibrationParams::S11System);
    replay.send(
        3,
        WorkerCommand::AdvanceCalibration(CalibrationPrompt::Open),
    );
    replay.expect_report(
        3,
        CalibrationKind::S11System,
        CalibrationPhase::Prompt(CalibrationPrompt::Short),
    );
    assert!(matches!(replay.event().event, WorkerEvent::Error(_)));
    replay.quiet_wire();
    replay.cancel_prompt(4, CalibrationKind::S11System, &token);
}

#[test]
fn cancelling_a_human_prompt_requires_exit_ack_and_a_fresh_device_fence() {
    for kind in [CalibrationKind::S11System, CalibrationKind::S21User] {
        let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
        let token = replay.start(2, params(kind));
        replay.cancel_prompt(3, kind, &token);
        replay.send(4, WorkerCommand::RunWorkspace(plan()));
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
    }
}

#[test]
fn early_or_wrong_completion_retires_before_event_backpressure() {
    for name in ["s11cal_completed", "s21cal_short", "s11UserCal_load"] {
        let mut replay = Replay::new(COMPLETE, 1, RecordingState::default());
        replay.start(2, CalibrationParams::S11System);
        replay.fill_events();
        replay.packet(name);
        replay.expect_wire(&["\x03", "$exit\n", "$s11,stop\n", "$local\n"]);
        assert!(matches!(replay.event().event, WorkerEvent::SweepStopped));
        replay.expect_report(2, CalibrationKind::S11System, CalibrationPhase::Unknown);
        assert!(matches!(
            replay.event().event,
            WorkerEvent::ConnectionLost(_)
        ));
    }
}

#[test]
fn cancel_during_measurement_is_unknown_and_releases_the_session() {
    let mut replay = Replay::new(COMPLETE, EVENT_CAPACITY, RecordingState::default());
    let token = replay.start(2, CalibrationParams::S21System);
    token.cancel();
    let measurement = replay.send(
        3,
        WorkerCommand::AdvanceCalibration(CalibrationPrompt::Through),
    );
    replay.expect_report(3, CalibrationKind::S21System, CalibrationPhase::Measuring);
    replay.expect_wire(&["$yes\n"]);
    measurement.cancel();
    replay.send(4, WorkerCommand::CancelCalibration);
    replay.expect_wire(&["\x03", "$exit\n", "$s21,stop\n", "$local\n"]);
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < WAIT);
        match replay.event().event {
            WorkerEvent::CalibrationReport(report) => {
                assert_eq!(report.phase, CalibrationPhase::Unknown)
            }
            WorkerEvent::ConnectionLost(_) => break,
            event => panic!("unexpected busy cancellation event: {event:?}"),
        }
    }
}

#[test]
fn rejected_operations_cannot_block_cancel_behind_full_events() {
    for operation in 0..4 {
        let mut replay = Replay::new(COMPLETE, 1, RecordingState::default());
        let token = replay.start(2, CalibrationParams::S11System);
        replay.fill_events();
        let command = match operation {
            0 => WorkerCommand::RefreshStatus,
            1 => WorkerCommand::RunWorkspace(plan()),
            2 => WorkerCommand::StartSource(SourceParams {
                kind: SourceKind::Rf,
                port: SourcePort::Port1,
                frequency_hz: 1_000_000_000,
                amplitude: SourceAmplitude::Dbm(-20),
                modulation: Modulation::Off,
            }),
            _ => WorkerCommand::AdvanceCalibration(CalibrationPrompt::Open),
        };
        replay.send(3, command);
        thread::sleep(Duration::from_millis(200));
        token.cancel();
        replay.send(4, WorkerCommand::CancelCalibration);
        replay.expect_wire(&["$exit\n", "$s11,stop\n", "$device\n"]);
        assert!(matches!(replay.event().event, WorkerEvent::SweepStopped));
        replay.expect_report(4, CalibrationKind::S11System, CalibrationPhase::Cancelled);
        replay.quiet_wire();
    }
}

#[test]
fn calibration_discards_an_incomplete_recording_pass() {
    let directory = tempfile::tempdir().unwrap();
    let mut plan = plan();
    plan.run.recording.enabled = true;
    plan.run.recording.directory = directory.path().to_owned();
    let mut replay = Replay::new(PREFIX, EVENT_CAPACITY, RecordingState::default());
    let sweep = replay.send(2, WorkerCommand::RunWorkspace(plan));
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
    sweep.cancel();
    replay.expect_wire(&["\x03", "$device\n"]);
    let token = replay.start(3, CalibrationParams::S11System);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    replay.cancel_prompt(4, CalibrationKind::S11System, &token);
}

#[test]
fn blocked_recording_write_does_not_delay_calibration_or_cancel() {
    let directory = tempfile::tempdir().unwrap();
    let mut plan = plan();
    plan.run.recording.enabled = true;
    plan.run.recording.directory = directory.path().to_owned();
    let (writing, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let writer = RecordWriter::with_test_save(move |key| {
        writing.send(key).unwrap();
        released
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
    let sweep = replay.send(2, WorkerCommand::RunWorkspace(plan));
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
    let token = replay.start(3, CalibrationParams::S21System);
    assert!(sweep.is_cancelled());
    replay.cancel_prompt(4, CalibrationKind::S21System, &token);
    release.send(()).unwrap();
    replay.quiet_wire();
    assert!(replay.events.try_recv().is_err());
}
