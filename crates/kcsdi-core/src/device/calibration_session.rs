// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Incremental legacy calibration with explicit human-standard confirmation.

use std::time::{Duration, Instant};

use super::{CLEANUP_TIMEOUT, COMMAND_GAP, Device};
use crate::calibration::{
    CalibrationKind, CalibrationParams, CalibrationPhase, CalibrationPrompt, CalibrationReport,
};
use crate::commands;
use crate::control::{CancellationToken, POLL_INTERVAL};
use crate::data::DeviceInfo;
use crate::error::{Error, Result};
use crate::protocol::{Packet, StreamMode};
use crate::transport::{GENERIC_TIMEOUT, Transport, remaining_timeout};

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const WARMUP_TIMEOUT: Duration = Duration::from_secs(10);
const YES: &[u8] = b"$yes\n";
pub(super) const EXIT: &[u8] = b"$exit\n";

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitingFor {
    Confirmation,
    SendConfirmation,
    FirstStandard,
    Open,
    Load,
    Completed,
    Human(CalibrationPrompt),
}

pub(super) struct CalibrationSession {
    kind: CalibrationKind,
    waiting: WaitingFor,
    started: Instant,
    timeout: Duration,
    measurement_timeout: Duration,
}

impl CalibrationSession {
    fn remaining(&self) -> Result<Duration> {
        if matches!(self.waiting, WaitingFor::Human(_)) {
            Ok(Duration::MAX)
        } else {
            remaining_timeout(self.started, self.timeout)
        }
    }
}

struct PacketNames {
    confirm: &'static str,
    warmup: &'static str,
    instruction: Option<&'static str>,
    first: &'static str,
    open: Option<&'static str>,
    load: Option<&'static str>,
    process: &'static str,
    save: Option<&'static str>,
    completed: &'static str,
    exit: &'static str,
}

fn names(kind: CalibrationKind) -> PacketNames {
    match kind {
        CalibrationKind::S11System => PacketNames {
            confirm: "s11cal_confirm",
            warmup: "s11cal_wait",
            instruction: Some("s11cal_step"),
            first: "s11cal_short",
            open: Some("s11cal_open"),
            load: Some("s11cal_load"),
            process: "s11cal_process",
            save: Some("s11cal_save"),
            completed: "s11cal_completed",
            exit: "s11cal_exit",
        },
        CalibrationKind::S21System => PacketNames {
            confirm: "s21cal_confirm",
            warmup: "s21cal_wait",
            instruction: Some("s21cal_step"),
            first: "s21cal_connect",
            open: None,
            load: None,
            process: "s21cal_process",
            save: Some("s21cal_save"),
            completed: "s21cal_completed",
            exit: "s21cal_exit",
        },
        CalibrationKind::S11User => PacketNames {
            confirm: "s11UserCal_confirm",
            warmup: "s11UserCal_wait",
            instruction: None,
            first: "s11UserCal_short",
            open: Some("s11UserCal_open"),
            load: Some("s11Usercal_load"),
            process: "s11UserCal_process",
            save: None,
            completed: "s11UserCal_completed",
            exit: "s11UserCal_exit",
        },
        CalibrationKind::S21User => PacketNames {
            confirm: "s21UserCal_confirm",
            warmup: "s21Usercal_wait",
            instruction: None,
            first: "s21UserCal_step",
            open: None,
            load: None,
            process: "s21UserCal_process",
            save: Some("s21UserCal_save"),
            completed: "s21UserCal_completed",
            exit: "s21UserCal_exit",
        },
    }
}

impl<T: Transport> Device<T> {
    pub fn calibration_report(&self) -> CalibrationReport {
        self.calibration_report
    }

    /// Start only after the caller has obtained consent to change calibration.
    /// The firmware confirmation is acknowledged by a later bounded poll.
    pub fn start_calibration_controlled(
        &mut self,
        params: &CalibrationParams,
        cancel: &CancellationToken,
    ) -> Result<CalibrationReport> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_calibration_idle()?;
        self.check_acquisition_source()?;
        let kind = params.kind();
        let command = params.command(&self.caps)?;
        let rbw = params.rbw(&self.caps);
        self.calibration_report = CalibrationReport {
            kind: Some(kind),
            phase: CalibrationPhase::Unknown,
        };
        self.calibration = Some(CalibrationSession {
            kind,
            waiting: WaitingFor::Confirmation,
            started: Instant::now(),
            timeout: CONFIRM_TIMEOUT,
            measurement_timeout: params.measurement_timeout(&self.caps),
        });
        self.release_on_close = true;
        let result = (|| {
            let started = Instant::now();
            for command in [
                commands::S11_STOP,
                commands::S21_STOP,
                commands::SPEC_STOP,
                commands::RF_SOURCE_STOP,
                commands::AF_SOURCE_STOP,
            ] {
                cancel.check()?;
                self.transport.send_with_timeout(
                    command.as_bytes(),
                    remaining_timeout(started, CLEANUP_TIMEOUT)?,
                )?;
            }
            self.reset_measurement();
            // Remember an attempted init before writing, including partial writes.
            self.active_mode = Some(kind.mode());
            cancel.check()?;
            self.transport.send_with_timeout(
                kind.init_command().as_bytes(),
                remaining_timeout(started, CLEANUP_TIMEOUT)?,
            )?;
            cancel.pause(COMMAND_GAP)?;
            self.transport.send_with_timeout(
                commands::set_rbw(rbw).as_bytes(),
                remaining_timeout(started, CLEANUP_TIMEOUT)?,
            )?;
            self.last_rbw = Some(rbw);
            cancel.check()?;
            let session = self.calibration.as_mut().expect("calibration owns setup");
            session.started = Instant::now();
            self.calibration_report.phase = CalibrationPhase::AwaitingConfirmation;
            self.transport
                .send_with_timeout(command.as_bytes(), CONFIRM_TIMEOUT)?;
            self.calibration
                .as_ref()
                .expect("calibration owns setup")
                .remaining()?;
            cancel.check()?;
            Ok(self.calibration_report)
        })();
        self.finish_calibration_action(result)
    }

    /// Read a bounded host window without imposing a ten-second line timeout.
    /// Human prompts have no deadline. Automatic phases retain their original
    /// total deadline across polls and intermediate progress packets.
    /// On error, close or drop the device to perform bounded cleanup. The
    /// failed session rejects further I/O and retains its cleanup ownership.
    pub fn poll_calibration_controlled(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<CalibrationReport> {
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        if self.calibration.is_none() {
            cancel.check()?;
            return Ok(self.calibration_report);
        }
        let result = self.poll_calibration_inner(cancel);
        if result.is_err() {
            self.calibration_report.phase = CalibrationPhase::Unknown;
            self.session_failed = true;
        }
        result
    }

    fn poll_calibration_inner(&mut self, cancel: &CancellationToken) -> Result<CalibrationReport> {
        let poll_started = Instant::now();
        loop {
            cancel.check()?;
            let Some(session) = &self.calibration else {
                return Ok(self.calibration_report);
            };
            let remaining = session.remaining()?;
            let Some(window) = POLL_INTERVAL
                .checked_sub(poll_started.elapsed())
                .filter(|time| !time.is_zero())
            else {
                return Ok(self.calibration_report);
            };
            if session.waiting == WaitingFor::SendConfirmation {
                self.confirm_calibration(window.min(remaining), cancel)?;
                continue;
            }
            let line = match self.transport.recv_line(window.min(remaining)) {
                Ok(line) => line,
                Err(Error::Timeout) => continue,
                Err(error) => return Err(error),
            };
            let packet = self.packets.feed_line(&line)?;
            self.calibration
                .as_ref()
                .expect("active poll")
                .remaining()?;
            cancel.check()?;
            if let Some(packet) = packet {
                self.accept_calibration_packet(&packet)?;
            }
        }
    }

    fn confirm_calibration(&mut self, window: Duration, cancel: &CancellationToken) -> Result<()> {
        cancel.check()?;
        let session = self
            .calibration
            .as_mut()
            .expect("confirmation has a session");
        session.remaining()?;
        session.waiting = WaitingFor::FirstStandard;
        session.started = Instant::now();
        session.timeout = WARMUP_TIMEOUT;
        self.calibration_report.phase = CalibrationPhase::WarmingUp;
        self.transport.send_with_timeout(YES, window)?;
        self.calibration
            .as_ref()
            .expect("confirmation has a session")
            .remaining()?;
        cancel.check()
    }

    /// Confirm exactly the standard currently requested by the instrument.
    /// A stale or duplicate UI action cannot send a second confirmation.
    pub fn advance_calibration_controlled(
        &mut self,
        expected_prompt: CalibrationPrompt,
        cancel: &CancellationToken,
    ) -> Result<CalibrationReport> {
        cancel.check()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        let Some(session) = self.calibration.as_mut() else {
            return Err(Error::InvalidParameter(
                "no calibration prompt is active".into(),
            ));
        };
        if session.waiting != WaitingFor::Human(expected_prompt) {
            return Err(Error::InvalidParameter(
                "calibration prompt has changed".into(),
            ));
        }
        session.waiting = match expected_prompt {
            CalibrationPrompt::Short => WaitingFor::Open,
            CalibrationPrompt::Open => WaitingFor::Load,
            CalibrationPrompt::Load | CalibrationPrompt::Through => WaitingFor::Completed,
        };
        session.started = Instant::now();
        session.timeout = session.measurement_timeout;
        self.calibration_report.phase = CalibrationPhase::Measuring;
        let budget = GENERIC_TIMEOUT.min(session.timeout);
        let result = (|| {
            self.transport.send_with_timeout(YES, budget)?;
            self.calibration
                .as_ref()
                .expect("measurement owns session")
                .remaining()?;
            cancel.check()?;
            Ok(self.calibration_report)
        })();
        self.finish_calibration_action(result)
    }

    /// EXIT is acknowledged only at a human prompt. Interrupting an automatic
    /// step retires the session and does not claim previous corrections survived.
    pub fn cancel_calibration_controlled(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<CalibrationReport> {
        cancel.check()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        let Some(session) = &self.calibration else {
            return Ok(self.calibration_report);
        };
        if !matches!(session.waiting, WaitingFor::Human(_)) {
            return self.finish_calibration_action(Err(Error::Cancelled));
        }
        let kind = session.kind;
        let result = (|| {
            let started = Instant::now();
            self.calibration_report.phase = CalibrationPhase::Unknown;
            self.transport.send_with_timeout(EXIT, CLEANUP_TIMEOUT)?;
            self.expect_calibration_boundary(names(kind).exit, started, CLEANUP_TIMEOUT, cancel)?;
            self.transport.send_with_timeout(
                kind.stop_command().as_bytes(),
                remaining_timeout(started, CLEANUP_TIMEOUT)?,
            )?;
            self.transport.send_with_timeout(
                commands::DEVICE.as_bytes(),
                remaining_timeout(started, CLEANUP_TIMEOUT)?,
            )?;
            let packet =
                self.expect_calibration_boundary("device", started, CLEANUP_TIMEOUT, cancel)?;
            DeviceInfo::from_packet(&packet)?;
            cancel.check()?;
            self.reset_measurement();
            self.calibration = None;
            self.calibration_report.phase = CalibrationPhase::Cancelled;
            Ok(self.calibration_report)
        })();
        self.finish_calibration_action(result)
    }

    pub(super) fn check_calibration_idle(&self) -> Result<()> {
        if self.calibration.is_some() {
            Err(Error::InvalidParameter(
                "finish or cancel calibration before another operation".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn expect_calibration_boundary(
        &mut self,
        expected: &str,
        started: Instant,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Packet> {
        loop {
            let line = self.recv_controlled(started, timeout, cancel)?;
            let packet = self.packets.feed_line(&line)?;
            remaining_timeout(started, timeout)?;
            cancel.check()?;
            if let Some(packet) = packet {
                if packet.is_error() {
                    return Err(Error::Device(packet.name));
                }
                if packet.name == expected {
                    return Ok(packet);
                }
                if packet.name.to_ascii_lowercase().contains("cal") {
                    return Err(Error::Protocol(format!(
                        "unexpected calibration packet {}",
                        packet.name
                    )));
                }
            }
        }
    }

    fn finish_calibration_action(
        &mut self,
        result: Result<CalibrationReport>,
    ) -> Result<CalibrationReport> {
        if result.is_err() {
            // No retry can distinguish a late standard prompt from a response
            // to a repeated YES. Preserve the first failure and require reconnect.
            self.calibration_report.phase = CalibrationPhase::Unknown;
            self.close();
        }
        result
    }

    fn accept_calibration_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.is_error() {
            return Err(Error::Device(packet.name.clone()));
        }
        let session = self
            .calibration
            .as_mut()
            .expect("packet belongs to active calibration");
        let names = names(session.kind);
        let name = packet.name.as_str();
        let prompt = match session.waiting {
            WaitingFor::FirstStandard if name == names.first => Some(match session.kind.mode() {
                StreamMode::S11 => CalibrationPrompt::Short,
                _ => CalibrationPrompt::Through,
            }),
            WaitingFor::Open if Some(name) == names.open => Some(CalibrationPrompt::Open),
            WaitingFor::Load if Some(name) == names.load => Some(CalibrationPrompt::Load),
            _ => None,
        };
        if let Some(prompt) = prompt {
            session.waiting = WaitingFor::Human(prompt);
            self.calibration_report.phase = CalibrationPhase::Prompt(prompt);
        } else if session.waiting == WaitingFor::Confirmation && name == names.confirm {
            session.waiting = WaitingFor::SendConfirmation;
        } else if session.waiting == WaitingFor::Completed && name == names.completed {
            // Completion is terminal (section 6), without a subsequent EXIT.
            // Retain the initialized measurement mode.
            self.calibration_report.phase = CalibrationPhase::Completed;
            self.calibration = None;
        } else if session.waiting == WaitingFor::FirstStandard
            && (name == names.warmup || Some(name) == names.instruction)
        {
            self.calibration_report.phase = CalibrationPhase::WarmingUp;
        } else if session.waiting == WaitingFor::Completed
            && name == names.process
            && self.calibration_report.phase != CalibrationPhase::Saving
        {
            self.calibration_report.phase = CalibrationPhase::Processing;
        } else if session.waiting == WaitingFor::Completed && Some(name) == names.save {
            self.calibration_report.phase = CalibrationPhase::Saving;
        } else if name.to_ascii_lowercase().contains("cal") {
            return Err(Error::Protocol(format!(
                "unexpected calibration packet {name}"
            )));
        }
        // Other packets can be residual telemetry or warnings. They cannot
        // advance this state machine or extend the active step deadline.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::thread;

    use crate::calibration::UserCalibrationParams;
    use crate::commands::{Cal, Format, Lo};
    use crate::device::{PointParams, PointSettings, S11Params, S21Params, SpecParams};
    use crate::model::{Model, Rbw};
    use crate::source::{
        Modulation, SourceAmplitude, SourceKind, SourceOutputState, SourceParams, SourcePort,
    };

    #[derive(Default)]
    struct Replay {
        lines: VecDeque<String>,
        sent: Vec<u8>,
        writes: Vec<Duration>,
        reads: Vec<Duration>,
        cancel_write: Option<(usize, CancellationToken)>,
        cancel_read: Option<(usize, CancellationToken)>,
        fail_write: Option<usize>,
        write_delay: Duration,
    }

    impl Replay {
        fn packet(&mut self, name: &str) {
            self.lines
                .extend([format!("$start,{name}"), "$fixture".into(), "$end".into()]);
        }

        fn identity(&mut self) {
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

        fn text(&self) -> String {
            String::from_utf8(self.sent.clone()).unwrap()
        }
    }

    impl Transport for Replay {
        fn send_with_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
            self.writes.push(timeout);
            if self.fail_write == Some(self.writes.len()) {
                return Err(Error::NotConnected);
            }
            let started = Instant::now();
            thread::sleep(self.write_delay);
            remaining_timeout(started, timeout)?;
            self.sent.extend_from_slice(data);
            if let Some((count, cancel)) = &self.cancel_write
                && *count == self.writes.len()
            {
                cancel.cancel();
            }
            Ok(())
        }

        fn recv_line(&mut self, timeout: Duration) -> Result<String> {
            self.reads.push(timeout);
            if let Some((count, cancel)) = &self.cancel_read
                && *count == self.reads.len()
            {
                cancel.cancel();
            }
            if let Some(line) = self.lines.pop_front() {
                return Ok(line);
            }
            thread::sleep(timeout);
            Err(Error::Timeout)
        }
    }

    fn params(kind: CalibrationKind) -> CalibrationParams {
        let user = UserCalibrationParams::from_range(1_000_000, 2_000_000, 201, Rbw::R10k).unwrap();
        match kind {
            CalibrationKind::S11System => CalibrationParams::S11System,
            CalibrationKind::S21System => CalibrationParams::S21System,
            CalibrationKind::S11User => CalibrationParams::S11User(user),
            CalibrationKind::S21User => CalibrationParams::S21User(user),
        }
    }

    fn kinds() -> [CalibrationKind; 4] {
        [
            CalibrationKind::S11System,
            CalibrationKind::S21System,
            CalibrationKind::S11User,
            CalibrationKind::S21User,
        ]
    }

    fn start_at_prompt(kind: CalibrationKind) -> Device<Replay> {
        let mut replay = Replay::default();
        replay.packet(names(kind).confirm);
        replay.packet(names(kind).warmup);
        if let Some(name) = names(kind).instruction {
            replay.packet(name);
        }
        replay.packet(names(kind).first);
        let mut device = Device::new(replay);
        let cancel = CancellationToken::default();
        assert_eq!(
            device
                .start_calibration_controlled(&params(kind), &cancel)
                .unwrap()
                .phase,
            CalibrationPhase::AwaitingConfirmation
        );
        let first = if kind.mode() == StreamMode::S11 {
            CalibrationPrompt::Short
        } else {
            CalibrationPrompt::Through
        };
        assert_eq!(
            device.poll_calibration_controlled(&cancel).unwrap().phase,
            CalibrationPhase::Prompt(first)
        );
        device
    }

    fn seed(kind: CalibrationKind, waiting: WaitingFor, phase: CalibrationPhase) -> Device<Replay> {
        let mut device = Device::new(Replay::default());
        device.calibration = Some(CalibrationSession {
            kind,
            waiting,
            started: Instant::now(),
            timeout: Duration::from_secs(30),
            measurement_timeout: Duration::from_secs(30),
        });
        device.calibration_report = CalibrationReport {
            kind: Some(kind),
            phase,
        };
        device.active_mode = Some(kind.mode());
        device.release_on_close = true;
        device
    }

    #[test]
    fn all_legacy_flows_match_case_and_require_each_standard_once() {
        for kind in kinds() {
            let cancel = CancellationToken::default();
            let mut device = start_at_prompt(kind);
            let mut expected = format!(
                "$s11,stop\n$s21,stop\n$spec,stop\n$rfsource,stop\n$afsource,stop\n{}{}{}$yes\n",
                kind.init_command(),
                commands::set_rbw(params(kind).rbw(&Model::Kc901V.capabilities())),
                params(kind).command(&Model::Kc901V.capabilities()).unwrap(),
            );
            assert_eq!(device.transport.text(), expected);
            let standards: &[CalibrationPrompt] = if kind.mode() == StreamMode::S11 {
                &[
                    CalibrationPrompt::Short,
                    CalibrationPrompt::Open,
                    CalibrationPrompt::Load,
                ]
            } else {
                &[CalibrationPrompt::Through]
            };
            for &prompt in standards {
                assert_eq!(
                    device
                        .advance_calibration_controlled(prompt, &cancel)
                        .unwrap()
                        .phase,
                    CalibrationPhase::Measuring
                );
                assert!(matches!(
                    device.advance_calibration_controlled(prompt, &cancel),
                    Err(Error::InvalidParameter(_))
                ));
                expected.push_str("$yes\n");
                match prompt {
                    CalibrationPrompt::Short => device.transport.packet(names(kind).open.unwrap()),
                    CalibrationPrompt::Open => device.transport.packet(names(kind).load.unwrap()),
                    CalibrationPrompt::Load | CalibrationPrompt::Through => {
                        device.transport.packet(names(kind).process);
                        if let Some(name) = names(kind).save {
                            device.transport.packet(name);
                        }
                        device.transport.packet(names(kind).completed);
                    }
                }
                device.poll_calibration_controlled(&cancel).unwrap();
                assert_eq!(device.transport.text(), expected);
            }
            assert_eq!(
                device.calibration_report().phase,
                CalibrationPhase::Completed
            );
            assert!(device.calibration.is_none());
            assert_eq!(device.active_mode, Some(kind.mode()));
            assert!(!device.requires_reconnect());
            assert!(!device.transport.text().contains("$exit"));
            assert!(
                device
                    .transport
                    .reads
                    .iter()
                    .all(|timeout| *timeout <= POLL_INTERVAL)
            );
            let sends = device.transport.writes.len();
            device.cancel_calibration_controlled(&cancel).unwrap();
            assert_eq!(device.transport.writes.len(), sends);
        }
    }

    #[test]
    fn calibration_start_always_reinitializes_and_user_points_convert_once() {
        let cancel = CancellationToken::default();
        let mut device = seed(
            CalibrationKind::S11User,
            WaitingFor::Completed,
            CalibrationPhase::Measuring,
        );
        device.transport.packet("s11UserCal_completed");
        assert_eq!(
            device.poll_calibration_controlled(&cancel).unwrap().phase,
            CalibrationPhase::Completed
        );
        device
            .start_calibration_controlled(&params(CalibrationKind::S11User), &cancel)
            .unwrap();
        assert_eq!(
            device.transport.text(),
            "$s11,stop\n$s21,stop\n$spec,stop\n$rfsource,stop\n$afsource,stop\n$s11,init\n$bw,10k\n$cal_user_s11,1500000,1000000,200\n"
        );
        assert!(!device.transport.text().contains("$yes"));
    }

    #[test]
    fn invalid_and_precancelled_starts_do_no_io() {
        let mut device = Device::new(Replay::default());
        let bad = CalibrationParams::S11User(UserCalibrationParams {
            center_hz: 0,
            span_hz: 0,
            points: 0,
            rbw: Rbw::R10k,
        });
        assert!(matches!(
            device.start_calibration_controlled(&bad, &CancellationToken::default()),
            Err(Error::InvalidParameter(_))
        ));
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(matches!(
            device.start_calibration_controlled(&CalibrationParams::S11System, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(device.calibration_report(), CalibrationReport::default());
        assert!(device.transport.writes.is_empty());
        assert!(device.transport.reads.is_empty());
    }

    #[test]
    fn calibration_excludes_requested_or_uncertain_source_before_io() {
        for state in [
            SourceOutputState::Requested(SourceKind::Rf),
            SourceOutputState::Unknown,
        ] {
            let mut device = Device::new(Replay::default());
            device.source.state = state;
            if matches!(state, SourceOutputState::Requested(_)) {
                device.source_kind = Some(SourceKind::Rf);
            }
            assert!(matches!(
                device.start_calibration_controlled(
                    &CalibrationParams::S11System,
                    &CancellationToken::default()
                ),
                Err(Error::InvalidParameter(_))
            ));
            assert!(device.transport.writes.is_empty());
            assert_eq!(device.source_report().state, state);
        }
    }

    #[test]
    fn calibration_excludes_ordinary_queries_sources_and_measurements() {
        let mut device = start_at_prompt(CalibrationKind::S11System);
        let writes = device.transport.writes.len();
        let reads = device.transport.reads.len();
        assert!(matches!(
            device.handshake(),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.device_info(),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.temperature(),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(device.voltage(), Err(Error::InvalidParameter(_))));
        assert!(matches!(
            device.stop_sweep(),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.start_calibration_controlled(
                &CalibrationParams::S21System,
                &CancellationToken::default()
            ),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.sweep_s11(&S11Params {
                cal: Cal::CalOff,
                format: Format::Loss,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: None
            }),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.sweep_s21(&S21Params {
                cal: Cal::CalOff,
                format: Format::Loss,
                lo: Lo::HighLo,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: None
            }),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.sweep_spec(&SpecParams {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                points: 3,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Rbw::R10k,
                ref_level_dbm: -10
            }),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.measure_point(&PointParams {
                frequency_hz: 1_000_000,
                settings: PointSettings::S11 {
                    cal: Cal::CalOff,
                    format: Format::Loss,
                    rbw: None
                }
            }),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.start_source(&SourceParams {
                kind: SourceKind::Rf,
                port: SourcePort::Port1,
                frequency_hz: 1_000_000,
                amplitude: SourceAmplitude::Dbm(-10),
                modulation: Modulation::Off
            }),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.stop_source(),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            device.poll_source_controlled(&CancellationToken::default()),
            Err(Error::InvalidParameter(_))
        ));
        assert_eq!(device.transport.writes.len(), writes);
        assert_eq!(device.transport.reads.len(), reads);
        assert_eq!(
            device.calibration_report().phase,
            CalibrationPhase::Prompt(CalibrationPrompt::Short)
        );
        assert!(!device.requires_reconnect());
    }

    #[test]
    fn wrong_prompt_and_precancelled_advance_preserve_the_human_prompt() {
        let mut device = start_at_prompt(CalibrationKind::S11User);
        let writes = device.transport.writes.len();
        assert!(matches!(
            device.advance_calibration_controlled(
                CalibrationPrompt::Load,
                &CancellationToken::default()
            ),
            Err(Error::InvalidParameter(_))
        ));
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(matches!(
            device.advance_calibration_controlled(CalibrationPrompt::Short, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(device.transport.writes.len(), writes);
        assert_eq!(
            device.calibration_report().phase,
            CalibrationPhase::Prompt(CalibrationPrompt::Short)
        );
        assert!(!device.requires_reconnect());
    }

    #[test]
    fn prompt_cancel_requires_its_exit_packet_and_fresh_identity() {
        for kind in kinds() {
            let mut device = start_at_prompt(kind);
            device.transport.packet(names(kind).exit);
            device.transport.identity();
            assert_eq!(
                device
                    .cancel_calibration_controlled(&CancellationToken::default())
                    .unwrap()
                    .phase,
                CalibrationPhase::Cancelled
            );
            assert!(
                device
                    .transport
                    .text()
                    .ends_with(&format!("$exit\n{}$device\n", kind.stop_command()))
            );
            assert_eq!(device.active_mode, None);
            assert!(device.calibration.is_none());
            assert!(!device.requires_reconnect());
        }
    }

    #[test]
    fn busy_cancel_is_uncertain_and_never_sends_a_recovery_query() {
        for (waiting, phase) in [
            (
                WaitingFor::Confirmation,
                CalibrationPhase::AwaitingConfirmation,
            ),
            (WaitingFor::FirstStandard, CalibrationPhase::WarmingUp),
            (WaitingFor::Open, CalibrationPhase::Measuring),
            (WaitingFor::Completed, CalibrationPhase::Processing),
            (WaitingFor::Completed, CalibrationPhase::Saving),
        ] {
            let mut device = seed(CalibrationKind::S11System, waiting, phase);
            assert!(matches!(
                device.cancel_calibration_controlled(&CancellationToken::default()),
                Err(Error::Cancelled)
            ));
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert!(device.requires_reconnect());
            assert_eq!(device.transport.text(), "\x03$exit\n$s11,stop\n$local\n");
        }
    }

    #[test]
    fn cancellation_during_initialization_and_advance_never_repeats_yes() {
        for write in 1..=8 {
            let cancel = CancellationToken::default();
            let replay = Replay {
                cancel_write: Some((write, cancel.clone())),
                ..Default::default()
            };
            let mut device = Device::new(replay);
            assert!(matches!(
                device.start_calibration_controlled(&CalibrationParams::S11System, &cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert!(device.requires_reconnect());
            assert!(!device.transport.text().contains("$yes"));
        }
        let cancel = CancellationToken::default();
        let mut device = seed(
            CalibrationKind::S11System,
            WaitingFor::Human(CalibrationPrompt::Short),
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
        );
        device.transport.cancel_write = Some((1, cancel.clone()));
        assert!(matches!(
            device.advance_calibration_controlled(CalibrationPrompt::Short, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(device.transport.text().matches("$yes\n").count(), 1);
        assert!(device.requires_reconnect());
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
    }

    #[test]
    fn cancelled_or_expired_confirmation_never_sends_yes() {
        for expired in [false, true] {
            let cancel = CancellationToken::default();
            let mut device = seed(
                CalibrationKind::S11System,
                WaitingFor::Confirmation,
                CalibrationPhase::AwaitingConfirmation,
            );
            device.transport.packet("s11cal_confirm");
            if expired {
                device.calibration.as_mut().unwrap().timeout = Duration::ZERO;
            } else {
                device.transport.cancel_read = Some((3, cancel.clone()));
            }
            assert!(device.poll_calibration_controlled(&cancel).is_err());
            assert!(!device.transport.text().contains("$yes"));
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert!(device.requires_reconnect());
            let (writes, reads) = (device.transport.writes.len(), device.transport.reads.len());
            assert!(matches!(
                device.poll_calibration_controlled(&CancellationToken::default()),
                Err(Error::NotConnected)
            ));
            assert_eq!(
                (device.transport.writes.len(), device.transport.reads.len()),
                (writes, reads)
            );
            assert!(device.calibration.is_some());
            device.close();
            assert!(
                device
                    .transport
                    .text()
                    .ends_with("\x03$exit\n$s11,stop\n$local\n")
            );
        }
    }

    #[test]
    fn confirmation_write_uses_the_poll_window_and_cancellation_is_terminal() {
        let cancel = CancellationToken::default();
        let mut device = seed(
            CalibrationKind::S21User,
            WaitingFor::Confirmation,
            CalibrationPhase::AwaitingConfirmation,
        );
        device.transport.packet("s21UserCal_confirm");
        device.transport.cancel_write = Some((1, cancel.clone()));
        assert!(matches!(
            device.poll_calibration_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(device.transport.writes[0] <= POLL_INTERVAL);
        assert_eq!(device.transport.text().matches("$yes\n").count(), 1);
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
        assert!(device.requires_reconnect());
    }

    #[test]
    fn user_load_case_and_foreign_or_early_completion_are_rejected() {
        for (waiting, name) in [
            (WaitingFor::Load, "s11UserCal_load"),
            (WaitingFor::Load, "s11cal_load"),
            (WaitingFor::Confirmation, "s11UserCal_completed"),
            (WaitingFor::Open, "s11UserCal_short"),
            (WaitingFor::Completed, "s21UserCal_completed"),
        ] {
            let mut device = seed(
                CalibrationKind::S11User,
                waiting,
                CalibrationPhase::Measuring,
            );
            device.transport.packet(name);
            assert!(matches!(
                device.poll_calibration_controlled(&CancellationToken::default()),
                Err(Error::Protocol(_))
            ));
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert!(device.requires_reconnect());
        }
    }

    #[test]
    fn progress_and_unrelated_packets_do_not_reset_the_step_deadline() {
        let mut device = seed(
            CalibrationKind::S21System,
            WaitingFor::Completed,
            CalibrationPhase::Measuring,
        );
        device.transport.packet("s21cal_process");
        device.transport.packet("warn_gtr");
        let started = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        device.calibration.as_mut().unwrap().started = started;
        device.calibration.as_mut().unwrap().timeout = Duration::from_secs(20);
        assert_eq!(
            device
                .poll_calibration_controlled(&CancellationToken::default())
                .unwrap()
                .phase,
            CalibrationPhase::Processing
        );
        assert_eq!(device.calibration.as_ref().unwrap().started, started);
        device.calibration.as_mut().unwrap().timeout = Duration::from_secs(10);
        device.transport.packet("s21cal_completed");
        assert!(matches!(
            device.poll_calibration_controlled(&CancellationToken::default()),
            Err(Error::Timeout)
        ));
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
        assert!(!device.transport.lines.is_empty());
    }

    #[test]
    fn human_prompt_has_no_deadline_but_still_observes_late_errors() {
        let mut device = seed(
            CalibrationKind::S11System,
            WaitingFor::Human(CalibrationPrompt::Short),
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
        );
        device.calibration.as_mut().unwrap().timeout = Duration::ZERO;
        assert_eq!(
            device
                .poll_calibration_controlled(&CancellationToken::default())
                .unwrap()
                .phase,
            CalibrationPhase::Prompt(CalibrationPrompt::Short)
        );
        device.transport.packet("err_uninit");
        assert!(
            matches!(device.poll_calibration_controlled(&CancellationToken::default()), Err(Error::Device(name)) if name == "err_uninit")
        );
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
        assert!(device.requires_reconnect());
    }

    #[test]
    fn cancellation_with_a_pending_exit_or_identity_never_retries_the_fence() {
        for cancel_write in [1, 3] {
            let cancel = CancellationToken::default();
            let mut device = seed(
                CalibrationKind::S21User,
                WaitingFor::Human(CalibrationPrompt::Through),
                CalibrationPhase::Prompt(CalibrationPrompt::Through),
            );
            device.transport.packet("s21UserCal_exit");
            device.transport.identity();
            device.transport.cancel_write = Some((cancel_write, cancel.clone()));
            assert!(matches!(
                device.cancel_calibration_controlled(&cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert!(device.requires_reconnect());
            assert_eq!(
                device.transport.text().matches("$device\n").count(),
                usize::from(cancel_write == 3)
            );
            assert!(!device.transport.lines.is_empty());
        }
    }

    #[test]
    fn failed_exit_fence_keeps_unknown_and_rejects_all_further_writes() {
        let mut device = seed(
            CalibrationKind::S11System,
            WaitingFor::Human(CalibrationPrompt::Short),
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
        );
        device.transport.packet("err_opt");
        device.transport.packet("s11cal_exit");
        device.transport.identity();
        assert!(
            matches!(device.cancel_calibration_controlled(&CancellationToken::default()), Err(Error::Device(name)) if name == "err_opt")
        );
        assert!(device.requires_reconnect());
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
        assert!(!device.transport.text().contains("$device"));
        let writes = device.transport.writes.len();
        assert!(matches!(
            device.start_calibration_controlled(
                &CalibrationParams::S11System,
                &CancellationToken::default()
            ),
            Err(Error::NotConnected)
        ));
        assert!(matches!(
            device.advance_calibration_controlled(
                CalibrationPrompt::Short,
                &CancellationToken::default()
            ),
            Err(Error::NotConnected)
        ));
        assert_eq!(device.transport.writes.len(), writes);
    }

    #[test]
    fn exit_cleanup_rejects_foreign_early_and_duplicate_calibration_packets() {
        for (packet, after_exit) in [
            ("s21cal_exit", false),
            ("s11cal_completed", false),
            ("s11cal_exit", true),
            ("s11cal_completed", true),
        ] {
            let mut device = seed(
                CalibrationKind::S11System,
                WaitingFor::Human(CalibrationPrompt::Short),
                CalibrationPhase::Prompt(CalibrationPrompt::Short),
            );
            if after_exit {
                device.transport.packet("s11cal_exit");
            }
            device.transport.packet(packet);
            device.transport.packet("s11cal_exit");
            device.transport.identity();
            assert!(matches!(
                device.cancel_calibration_controlled(&CancellationToken::default()),
                Err(Error::Protocol(_))
            ));
            assert!(device.requires_reconnect());
            assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
            assert_eq!(
                device.transport.text().matches("$device\n").count(),
                usize::from(after_exit)
            );
            assert!(!device.transport.lines.is_empty());
        }
    }

    #[test]
    fn completed_report_survives_later_close_failure() {
        let mut device = seed(
            CalibrationKind::S11System,
            WaitingFor::Completed,
            CalibrationPhase::Saving,
        );
        device.transport.packet("s11cal_completed");
        assert_eq!(
            device
                .poll_calibration_controlled(&CancellationToken::default())
                .unwrap()
                .phase,
            CalibrationPhase::Completed
        );
        device.transport.fail_write = Some(1);
        device.close();
        assert_eq!(
            device.calibration_report().phase,
            CalibrationPhase::Completed
        );
        assert!(device.requires_reconnect());
        assert_eq!(device.transport.text(), "$local\n");
    }

    #[test]
    fn active_close_uses_one_cleanup_budget_and_no_read() {
        let mut device = seed(
            CalibrationKind::S11System,
            WaitingFor::Open,
            CalibrationPhase::Measuring,
        );
        device.transport.write_delay = Duration::from_millis(3);
        device.close();
        assert_eq!(device.transport.text(), "\x03$exit\n$s11,stop\n$local\n");
        assert!(
            device
                .transport
                .writes
                .windows(2)
                .all(|pair| pair[1] < pair[0])
        );
        assert!(device.transport.reads.is_empty());
        assert!(device.requires_reconnect());
        assert_eq!(device.calibration_report().phase, CalibrationPhase::Unknown);
    }

    #[test]
    fn inactive_poll_and_cancel_do_no_io() {
        let mut device = Device::new(Replay::default());
        assert_eq!(
            device
                .poll_calibration_controlled(&CancellationToken::default())
                .unwrap(),
            CalibrationReport::default()
        );
        assert_eq!(
            device
                .cancel_calibration_controlled(&CancellationToken::default())
                .unwrap(),
            CalibrationReport::default()
        );
        assert!(device.transport.reads.is_empty());
        assert!(device.transport.writes.is_empty());
    }
}
