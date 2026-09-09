// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! High-level device session API.
//!
//! A [`Device`] wraps a [`Transport`] and implements the session flow from
//! the protocol reference: handshake with `C`, query identity and status
//! packets, run sweeps (`stop` -> `init` -> `run` -> consume the stream
//! until `end`), and exit remote mode with `$local` on close.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::commands::{self, Cal, Format, Lo, ScanMode};
use crate::control::{CancellationToken, POLL_INTERVAL};
use crate::data::{DeviceInfo, SweepData, SweepPoint, Voltage, parse_f64, telemetry_values};
use crate::error::{Error, Result};
use crate::model::{self, Capabilities, Model, Rbw};
use crate::protocol::{Packet, PacketParser, StreamEvent, StreamMode, StreamParser};
use crate::source::{SourceKind, SourceOutputState, SourceReport};
use crate::transport::{GENERIC_TIMEOUT, TcpTransport, Transport, remaining_timeout};

mod source_session;

/// Conservative mode-control pacing, exercised on KC901V V1.6.1.
/// This is not a documented minimum delay for every command.
const COMMAND_GAP: Duration = Duration::from_millis(100);

/// One host budget for interrupt synchronization or best-effort close.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

fn stop_command(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::S11 => commands::S11_STOP,
        StreamMode::S21 => commands::S21_STOP,
        StreamMode::Spec => commands::SPEC_STOP,
        _ => unreachable!("only S11, S21 and SPEC sessions are implemented"),
    }
}

/// A validated prefix, not a complete measurement. Only a successful sweep
/// result is suitable for holds, analysis or export.
#[derive(Debug, Clone, Copy)]
pub struct SweepProgress<'a> {
    pub mode: StreamMode,
    pub format: &'a str,
    pub points: &'a [SweepPoint],
    pub expected_points: u32,
}

/// Parameters of an S11 sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S11Params {
    pub cal: Cal,
    pub format: Format,
    /// Returned samples including both endpoints, not the wire count.
    pub points: u32,
    pub start_hz: u64,
    pub stop_hz: u64,
    /// When set, `$bw,<rbw>` is pushed before the run and the RBW factor is
    /// used for the sweep timeout. Otherwise the last selected bandwidth
    /// is used, or the slowest model bandwidth when it is unknown.
    pub rbw: Option<Rbw>,
}

/// Parameters of an S21 transmission sweep (section 3.5).
/// Delay values are returned in seconds, without display-unit conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S21Params {
    pub cal: Cal,
    pub format: Format,
    pub lo: Lo,
    /// Returned samples including both endpoints, not the wire count.
    pub points: u32,
    pub start_hz: u64,
    pub stop_hz: u64,
    /// Explicit bandwidth, or reuse the last selected bandwidth as in S11.
    pub rbw: Option<Rbw>,
}

/// Parameters of a spectrum sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecParams {
    pub cal: Cal,
    pub lo: Lo,
    /// Returned samples including both endpoints, not the wire count.
    pub points: u32,
    pub start_hz: u64,
    pub stop_hz: u64,
    pub rbw: Rbw,
    pub ref_level_dbm: i32,
}

/// Receiver settings for a single-frequency acquisition (sections 3.4, 3.5
/// and 3.8). The host interrupts the continuous stream after one full frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointSettings {
    S11 {
        cal: Cal,
        format: Format,
        rbw: Option<Rbw>,
    },
    S21 {
        cal: Cal,
        format: Format,
        lo: Lo,
        rbw: Option<Rbw>,
    },
    Spec {
        cal: Cal,
        lo: Lo,
        rbw: Rbw,
        ref_level_dbm: i32,
    },
}

impl PointSettings {
    pub fn mode(&self) -> StreamMode {
        match self {
            Self::S11 { .. } => StreamMode::S11,
            Self::S21 { .. } => StreamMode::S21,
            Self::Spec { .. } => StreamMode::Spec,
        }
    }

    pub fn format(&self) -> &'static str {
        self.measurement_format().map_or("", Format::as_str)
    }

    fn measurement_format(&self) -> Option<Format> {
        match self {
            Self::S11 { format, .. } | Self::S21 { format, .. } => Some(*format),
            Self::Spec { .. } => None,
        }
    }
}

/// One requested frequency, independent of finite sweep span and point count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointParams {
    pub settings: PointSettings,
    pub frequency_hz: u64,
}

/// A remote-control session with a KC901 instrument.
pub struct Device<T: Transport> {
    transport: T,
    remote: bool,
    release_on_close: bool,
    packets: PacketParser,
    streams: StreamParser,
    caps: Capabilities,
    active_mode: Option<StreamMode>,
    last_rbw: Option<Rbw>,
    session_failed: bool,
    source: SourceReport,
    source_kind: Option<SourceKind>,
}

impl Device<TcpTransport> {
    /// Connect over TCP and perform the handshake (send `C`, wait for the
    /// `id` packet). Returns the ready session.
    pub fn connect(host: &str, port: u16) -> Result<Self> {
        Self::connect_with_model(host, port, Model::Kc901V)
    }

    /// Connect using explicitly selected model capabilities. The identity
    /// packet does not reliably identify the model, so no auto-detection is
    /// implied by the KC901V default.
    pub fn connect_with_model(host: &str, port: u16, model: Model) -> Result<Self> {
        Self::connect_with_model_controlled(host, port, model, &CancellationToken::default())
    }

    /// Connect and handshake with cancellation between bounded I/O waits.
    pub fn connect_with_model_controlled(
        host: &str,
        port: u16,
        model: Model,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let transport = TcpTransport::connect_controlled(host, port, cancel)?;
        let mut device = Self::with_model(transport, model);
        device.handshake_controlled(cancel)?;
        Ok(device)
    }
}

impl<T: Transport> Device<T> {
    /// Wrap an already-connected transport. No handshake is performed; call
    /// [`Device::handshake`] explicitly.
    pub fn new(transport: T) -> Self {
        Self::with_model(transport, Model::Kc901V)
    }

    pub fn with_model(transport: T, model: Model) -> Self {
        Self {
            transport,
            remote: false,
            release_on_close: false,
            packets: PacketParser::new(),
            streams: StreamParser::new(),
            caps: model.capabilities(),
            active_mode: None,
            last_rbw: None,
            session_failed: false,
            source: SourceReport::default(),
            source_kind: None,
        }
    }

    /// Transport, framing or cleanup failures require a fresh connection.
    /// A stop write alone does not establish that buffered replies are drained.
    pub fn requires_reconnect(&self) -> bool {
        self.session_failed
    }

    fn record_result<U>(&mut self, result: Result<U>) -> Result<U> {
        if result.is_err() && self.source_kind.is_some() {
            self.source.state = SourceOutputState::Unknown;
        }
        if matches!(
            &result,
            Err(Error::Io(_) | Error::Protocol(_) | Error::Timeout | Error::NotConnected)
        ) {
            self.session_failed = true;
        }
        result
    }

    /// Send `C` and wait for the identity reply. Returns the serial number.
    ///
    /// Accepts both the verified `$start,id` packet form and the manual's
    /// `[KC901]<serial>` plain-text form (doc 1.4). A `ConFail` packet maps
    /// to [`Error::DeviceBusy`].
    pub fn handshake(&mut self) -> Result<String> {
        self.handshake_controlled(&CancellationToken::default())
    }

    /// Perform the handshake without accepting a reply after cancellation.
    pub fn handshake_controlled(&mut self, cancel: &CancellationToken) -> Result<String> {
        cancel.check()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        let result = self.handshake_inner(GENERIC_TIMEOUT, cancel);
        if matches!(result, Err(Error::Cancelled)) {
            self.session_failed = true;
        }
        self.record_result(result)
    }

    #[cfg(test)]
    fn handshake_with_timeout(&mut self, timeout: Duration) -> Result<String> {
        self.handshake_inner(timeout, &CancellationToken::default())
    }

    fn handshake_inner(&mut self, timeout: Duration, cancel: &CancellationToken) -> Result<String> {
        cancel.check()?;
        let started = Instant::now();
        self.transport
            .send_with_timeout(commands::HANDSHAKE, timeout)?;
        // A cancelled or late handshake reply must still release the remote
        // session. ConFail explicitly means the handshake was refused.
        self.release_on_close = true;
        loop {
            let line = self.recv_controlled(started, timeout, cancel)?;
            if let Some(serial) = line.strip_prefix("[KC901]") {
                cancel.check()?;
                self.remote = true;
                return Ok(serial.trim().to_string());
            }
            let packet = self.packets.feed_line(&line)?;
            remaining_timeout(started, timeout)?;
            if packet
                .as_ref()
                .is_some_and(|packet| packet.name == "ConFail")
            {
                self.release_on_close = false;
            }
            cancel.check()?;
            if let Some(packet) = packet {
                match packet.name.as_str() {
                    "id" => {
                        self.remote = true;
                        return Ok(packet
                            .args
                            .first()
                            .and_then(|row| row.first())
                            .cloned()
                            .unwrap_or_default());
                    }
                    "ConFail" => {
                        return Err(Error::DeviceBusy(
                            "front panel window open, exit it first".into(),
                        ));
                    }
                    name if name.starts_with("err_") => {
                        return Err(Error::Device(name.to_string()));
                    }
                    name => log::debug!("handshake: ignoring packet {name}"),
                }
            }
        }
    }

    /// `$device` -> parsed identity information.
    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        self.device_info_controlled(&CancellationToken::default())
    }

    pub fn device_info_controlled(&mut self, cancel: &CancellationToken) -> Result<DeviceInfo> {
        let result = self
            .query_controlled(commands::DEVICE, "device", GENERIC_TIMEOUT, cancel)
            .and_then(|packet| DeviceInfo::from_packet(&packet));
        self.record_result(result)
    }

    /// `$temp` -> internal temperature in deg C.
    pub fn temperature(&mut self) -> Result<f64> {
        self.temperature_controlled(&CancellationToken::default())
    }

    pub fn temperature_controlled(&mut self, cancel: &CancellationToken) -> Result<f64> {
        let result = self
            .query_controlled(commands::TEMP, "temp", GENERIC_TIMEOUT, cancel)
            .and_then(|packet| telemetry_values(&packet, "temp").map(|[value]| value));
        self.record_result(result)
    }

    /// `$voltage` -> external and battery voltages.
    pub fn voltage(&mut self) -> Result<Voltage> {
        self.voltage_controlled(&CancellationToken::default())
    }

    pub fn voltage_controlled(&mut self, cancel: &CancellationToken) -> Result<Voltage> {
        let result = self
            .query_controlled(commands::VOLTAGE, "voltage", GENERIC_TIMEOUT, cancel)
            .and_then(|packet| Voltage::from_packet(&packet));
        self.record_result(result)
    }

    /// Run an S11 sweep: `stop` -> `init` -> optional `$bw` -> `run`, then
    /// consume the data stream until `$end`.
    pub fn sweep_s11(&mut self, params: &S11Params) -> Result<SweepData> {
        self.sweep_s11_controlled(params, &CancellationToken::default(), |_| {})
    }

    /// Collect a complete sweep while reporting replaceable validated prefixes.
    pub fn sweep_s11_controlled(
        &mut self,
        params: &S11Params,
        cancel: &CancellationToken,
        progress: impl FnMut(SweepProgress<'_>),
    ) -> Result<SweepData> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_acquisition_source()?;
        let wire_points = self.caps.wire_points(params.points)?;
        let result = (|| {
            self.prepare_mode(StreamMode::S11, cancel)?;
            if let Some(rbw) = params.rbw {
                self.set_rbw(rbw, cancel)?;
            }
            let run = commands::s11_run(
                params.cal,
                params.format,
                wire_points,
                ScanMode::StartStop,
                params.start_hz,
                Some(params.stop_hz),
            );
            self.send_controlled(run.as_bytes(), cancel)?;
            self.collect_controlled(
                StreamMode::S11,
                Some(params.format),
                params.points,
                self.sweep_timeout(params.points),
                cancel,
                progress,
            )
        })();
        self.finish_sweep(result)
    }

    /// Run a finite S21 sweep using the source-based model limits (section 7.3).
    pub fn sweep_s21(&mut self, params: &S21Params) -> Result<SweepData> {
        self.sweep_s21_controlled(params, &CancellationToken::default(), |_| {})
    }

    /// Collect a complete transmission sweep with cancellation and prefixes.
    pub fn sweep_s21_controlled(
        &mut self,
        params: &S21Params,
        cancel: &CancellationToken,
        progress: impl FnMut(SweepProgress<'_>),
    ) -> Result<SweepData> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_acquisition_source()?;
        let wire_points = self.caps.wire_points(params.points)?;
        let result = (|| {
            self.prepare_mode(StreamMode::S21, cancel)?;
            if let Some(rbw) = params.rbw {
                self.set_rbw(rbw, cancel)?;
            }
            let run = commands::s21_run(
                params.cal,
                params.format,
                params.lo,
                wire_points,
                ScanMode::StartStop,
                params.start_hz,
                Some(params.stop_hz),
            );
            self.send_controlled(run.as_bytes(), cancel)?;
            self.collect_controlled(
                StreamMode::S21,
                Some(params.format),
                params.points,
                self.sweep_timeout(params.points),
                cancel,
                progress,
            )
        })();
        self.finish_sweep(result)
    }

    /// Run a spectrum sweep: `stop` -> `init` -> `$bw` -> `$specref` ->
    /// `run`, then consume the data stream until `$end`.
    pub fn sweep_spec(&mut self, params: &SpecParams) -> Result<SweepData> {
        self.sweep_spec_controlled(params, &CancellationToken::default(), |_| {})
    }

    /// Collect a complete spectrum sweep with cancellation and prefix updates.
    pub fn sweep_spec_controlled(
        &mut self,
        params: &SpecParams,
        cancel: &CancellationToken,
        progress: impl FnMut(SweepProgress<'_>),
    ) -> Result<SweepData> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_acquisition_source()?;
        let wire_points = self.caps.wire_points(params.points)?;
        let result = (|| {
            self.prepare_mode(StreamMode::Spec, cancel)?;
            self.set_rbw(params.rbw, cancel)?;
            self.send_controlled(
                commands::set_spec_ref(params.ref_level_dbm).as_bytes(),
                cancel,
            )?;
            let run = commands::spec_run(
                params.cal,
                params.lo,
                wire_points,
                ScanMode::StartStop,
                params.start_hz,
                Some(params.stop_hz),
                None,
            );
            self.send_controlled(run.as_bytes(), cancel)?;
            self.collect_controlled(
                StreamMode::Spec,
                None,
                params.points,
                self.sweep_timeout(params.points),
                cancel,
                progress,
            )
        })();
        self.finish_sweep(result)
    }

    /// Acquire one complete point and interrupt the continuous output before
    /// returning it. Reported frequency and raw values remain unchanged.
    /// Point limits follow the source-based mode tables, not a finite sweep.
    pub fn measure_point(&mut self, params: &PointParams) -> Result<SweepData> {
        self.measure_point_controlled(params, &CancellationToken::default())
    }

    /// No point is published until an interrupt and fresh identity reply
    /// establish the receive boundary. Failed synchronization retires the
    /// session. Cancellation also discards a point captured before cleanup.
    pub fn measure_point_controlled(
        &mut self,
        params: &PointParams,
        cancel: &CancellationToken,
    ) -> Result<SweepData> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_acquisition_source()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        let result = (|| {
            self.prepare_mode(params.settings.mode(), cancel)?;
            let frequency = params.frequency_hz;
            let run = match params.settings {
                PointSettings::S11 { cal, format, rbw } => {
                    if let Some(rbw) = rbw {
                        self.set_rbw(rbw, cancel)?;
                    }
                    commands::s11_run(cal, format, 1, ScanMode::StartStop, frequency, None)
                }
                PointSettings::S21 {
                    cal,
                    format,
                    lo,
                    rbw,
                } => {
                    if let Some(rbw) = rbw {
                        self.set_rbw(rbw, cancel)?;
                    }
                    commands::s21_run(cal, format, lo, 1, ScanMode::StartStop, frequency, None)
                }
                PointSettings::Spec {
                    cal,
                    lo,
                    rbw,
                    ref_level_dbm,
                } => {
                    self.set_rbw(rbw, cancel)?;
                    self.send_controlled(commands::set_spec_ref(ref_level_dbm).as_bytes(), cancel)?;
                    commands::spec_run(cal, lo, 1, ScanMode::StartStop, frequency, None, None)
                }
            };
            self.send_controlled(run.as_bytes(), cancel)?;
            self.collect_controlled(
                params.settings.mode(),
                params.settings.measurement_format(),
                1,
                self.sweep_timeout(1),
                cancel,
                |_| {},
            )
        })();
        // Framing and transport failures remain terminal even if the cleanup
        // reply arrives. A full point alone never ends continuous acquisition.
        let result = self.record_result(result);
        let cleanup = self.cancel_sweep(CLEANUP_TIMEOUT);
        match result {
            Err(Error::Cancelled) => {
                cleanup?;
                Err(Error::Cancelled)
            }
            Err(error) => Err(error),
            Ok(data) => {
                cleanup?;
                cancel.check()?;
                Ok(data)
            }
        }
    }

    fn send_controlled(&mut self, data: &[u8], cancel: &CancellationToken) -> Result<()> {
        cancel.check()?;
        self.transport.send(data)?;
        cancel.check()
    }

    fn set_rbw(&mut self, rbw: Rbw, cancel: &CancellationToken) -> Result<()> {
        self.send_controlled(commands::set_rbw(rbw).as_bytes(), cancel)?;
        self.last_rbw = Some(rbw);
        Ok(())
    }

    fn sweep_timeout(&self, points: u32) -> Duration {
        model::sweep_timeout(
            self.last_rbw.or(self.caps.rbw_list.first().copied()),
            points,
        )
    }

    fn finish_sweep(&mut self, result: Result<SweepData>) -> Result<SweepData> {
        if matches!(result, Err(Error::Cancelled)) {
            self.cancel_sweep(CLEANUP_TIMEOUT)?;
            return Err(Error::Cancelled);
        }
        let result = self.record_result(result);
        if result.is_err() {
            // Preserve the original failure. A fully framed device error can
            // recover by reinitializing, but broken sessions need reconnecting.
            if self.stop_sweep().is_err() {
                self.session_failed = true;
            }
            self.reset_measurement();
        }
        result
    }

    /// Stop the initialized measurement mode. This also permits switching
    /// modes without a mode-conflict error (sections 3.4, 3.5, 3.8 and 12).
    pub fn stop_sweep(&mut self) -> Result<()> {
        if let Some(mode) = self.active_mode {
            let result = self.transport.send(stop_command(mode).as_bytes());
            self.record_result(result)?;
            sleep(COMMAND_GAP);
            self.active_mode = None;
        }
        Ok(())
    }

    fn prepare_mode(&mut self, mode: StreamMode, cancel: &CancellationToken) -> Result<()> {
        cancel.check()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        self.check_acquisition_source()?;
        if self.active_mode == Some(mode) {
            return Ok(());
        }
        if self.active_mode.is_some() {
            self.stop_sweep()?;
        } else {
            // Front-panel or previous-session mode state is unknown.
            for command in [commands::S11_STOP, commands::S21_STOP, commands::SPEC_STOP] {
                self.send_controlled(command.as_bytes(), cancel)?;
                cancel.pause(COMMAND_GAP)?;
            }
        }
        let init = match mode {
            StreamMode::S11 => commands::S11_INIT,
            StreamMode::S21 => commands::S21_INIT,
            StreamMode::Spec => commands::SPEC_INIT,
            _ => unreachable!("only S11, S21 and SPEC sessions are implemented"),
        };
        self.send_controlled(init.as_bytes(), cancel)?;
        self.active_mode = Some(mode);
        cancel.pause(COMMAND_GAP)?;
        Ok(())
    }

    fn reset_measurement(&mut self) {
        self.active_mode = None;
        self.last_rbw = None;
        self.packets = PacketParser::new();
        self.streams = StreamParser::new();
    }

    fn check_acquisition_source(&self) -> Result<()> {
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        if self.source_kind.is_some() || self.source.state == SourceOutputState::Unknown {
            return Err(Error::InvalidParameter(
                "stop the signal source before starting an acquisition".into(),
            ));
        }
        Ok(())
    }

    fn cancel_sweep(&mut self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        self.reset_measurement();
        let result = (|| {
            self.transport
                .send_with_timeout(commands::ABORT, remaining_timeout(started, timeout)?)?;
            self.transport.send_with_timeout(
                commands::DEVICE.as_bytes(),
                remaining_timeout(started, timeout)?,
            )?;
            // Synchronize against ordered command execution (sections 1.1
            // and 1.4). Require a fresh identity reply after the interrupt,
            // not merely a stop write or a leftover measurement end.
            let packet =
                self.expect_controlled("device", started, timeout, &CancellationToken::default())?;
            DeviceInfo::from_packet(&packet)?;
            Ok(())
        })();
        self.reset_measurement();
        if result.is_err() {
            self.session_failed = true;
        }
        result
    }

    /// Exit remote mode (`$local`). Best effort; also called from `Drop`.
    pub fn close(&mut self) {
        self.session_failed = true;
        let source_stop_needed = matches!(
            self.source.state,
            SourceOutputState::Requested(_) | SourceOutputState::Unknown
        );
        if self.source.state != SourceOutputState::NotStarted {
            self.source.state = SourceOutputState::Unknown;
        }
        if self.remote || self.release_on_close {
            let started = Instant::now();
            if source_stop_needed {
                for &kind in self.source_stop_kinds() {
                    if let Ok(remaining) = remaining_timeout(started, CLEANUP_TIMEOUT) {
                        let _ = self
                            .transport
                            .send_with_timeout(kind.stop_command().as_bytes(), remaining);
                    }
                }
            }
            if let Some(mode) = self.active_mode
                && let Ok(remaining) = remaining_timeout(started, CLEANUP_TIMEOUT)
            {
                let _ = self
                    .transport
                    .send_with_timeout(stop_command(mode).as_bytes(), remaining);
            }
            if let Ok(remaining) = remaining_timeout(started, CLEANUP_TIMEOUT) {
                let _ = self
                    .transport
                    .send_with_timeout(commands::LOCAL.as_bytes(), remaining);
            }
            self.remote = false;
            self.release_on_close = false;
            self.source_kind = None;
            self.reset_measurement();
        }
    }

    fn recv_controlled(
        &mut self,
        started: Instant,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let line_started = Instant::now();
        loop {
            cancel.check()?;
            let remaining = remaining_timeout(started, timeout)?
                .min(remaining_timeout(line_started, GENERIC_TIMEOUT)?)
                .min(POLL_INTERVAL);
            let result = self.transport.recv_line(remaining);
            remaining_timeout(started, timeout)?;
            remaining_timeout(line_started, GENERIC_TIMEOUT)?;
            match result {
                Err(Error::Timeout) => cancel.check()?,
                // Callers inspect complete framing before checking cancellation.
                // A fully received handshake refusal must not be lost here.
                result => return result,
            }
        }
    }

    /// Sending and reading share one budget. Unrelated replies do not extend it.
    #[cfg(test)]
    fn query_packet(&mut self, command: &str, name: &str, timeout: Duration) -> Result<Packet> {
        self.query_controlled(command, name, timeout, &CancellationToken::default())
    }

    fn query_controlled(
        &mut self,
        command: &str,
        name: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Packet> {
        cancel.check()?;
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        let started = Instant::now();
        let result = self
            .transport
            .send_with_timeout(command.as_bytes(), timeout)
            .and_then(|()| self.expect_controlled(name, started, timeout, cancel));
        if matches!(result, Err(Error::Cancelled)) {
            // An interrupted query can leave a same-name reply pending.
            self.session_failed = true;
        }
        if result.is_err() && self.source_kind.is_some() {
            // A pending query reply cannot become a later source stop fence.
            self.session_failed = true;
        }
        self.record_result(result)
    }

    /// Read until the expected packet completes. Device errors abort the query.
    #[cfg(test)]
    fn expect_packet(&mut self, name: &str, started: Instant, timeout: Duration) -> Result<Packet> {
        self.expect_controlled(name, started, timeout, &CancellationToken::default())
    }

    fn expect_controlled(
        &mut self,
        name: &str,
        started: Instant,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Packet> {
        loop {
            let line = self.recv_controlled(started, timeout, cancel)?;
            let packet = self.packets.feed_line(&line)?;
            if let Some(packet) = &packet {
                self.note_source_packet(packet);
            }
            remaining_timeout(started, timeout)?;
            cancel.check()?;
            if let Some(packet) = packet {
                if packet.is_error() {
                    return Err(Error::Device(packet.name));
                }
                if packet.name == name {
                    return Ok(packet);
                }
                if self.source_kind.is_none() && self.source.state != SourceOutputState::Unknown {
                    // A synchronous logger must not hold source control open.
                    log::warn!("unexpected packet {}, waiting for {name}", packet.name);
                }
            }
        }
    }

    /// Collect one complete frame with the requested schema and sample count.
    /// Reported frequencies remain authoritative, including repeated rounded
    /// values and endpoint overshoot (sections 4.2 and 12.4).
    #[cfg(test)]
    fn collect_stream(
        &mut self,
        mode: StreamMode,
        format: Option<Format>,
        expected_points: u32,
        timeout: Duration,
    ) -> Result<SweepData> {
        self.collect_controlled(
            mode,
            format,
            expected_points,
            timeout,
            &CancellationToken::default(),
            |_| {},
        )
    }

    fn collect_controlled(
        &mut self,
        mode: StreamMode,
        format: Option<Format>,
        expected_points: u32,
        timeout: Duration,
        cancel: &CancellationToken,
        mut progress: impl FnMut(SweepProgress<'_>),
    ) -> Result<SweepData> {
        let started = Instant::now();
        let expected_format = format.map_or("", Format::as_str);
        let expected_header = format.map_or_else(
            || mode.name().to_string(),
            |format| format!("{},{format}", mode.name()),
        );
        let value_columns = match format {
            Some(Format::Ri | Format::Ma) => 2,
            Some(Format::Z) => 3,
            _ => 1,
        };
        let mut points: Vec<SweepPoint> = Vec::new();
        loop {
            let line = self.recv_controlled(started, timeout, cancel)?;
            // Feed the packet parser too, so err_* packets interleaved with
            // a stream are still caught.
            let packet = self.packets.feed_line(&line)?;
            remaining_timeout(started, timeout)?;
            cancel.check()?;
            if let Some(packet) = &packet {
                if packet.is_error() {
                    return Err(Error::Device(packet.name.clone()));
                }
                let packet_mode = packet
                    .name
                    .split(',')
                    .next()
                    .and_then(StreamMode::from_name);
                if packet_mode.is_some() && packet.name != expected_header {
                    return Err(Error::Protocol(format!(
                        "unexpected measurement header {}, expected {expected_header}",
                        packet.name
                    )));
                }
            }
            match self.streams.feed_line(&line) {
                Some(StreamEvent::Start { mode: m, format: f }) => {
                    if m != mode || f != expected_format {
                        return Err(Error::Protocol(format!(
                            "unexpected measurement stream {},{f}, expected {expected_header}",
                            m.name()
                        )));
                    }
                    // A new frame replaces an incomplete one. Never combine
                    // data from before and after parser resynchronization.
                    points.clear();
                    progress(SweepProgress {
                        mode,
                        format: expected_format,
                        points: &points,
                        expected_points,
                    });
                }
                Some(StreamEvent::Data {
                    mode: m, fields, ..
                }) if m == mode => {
                    if points.len() >= expected_points as usize {
                        return Err(Error::Protocol(format!(
                            "measurement exceeds the requested {expected_points} samples"
                        )));
                    }
                    if fields.len() != value_columns + 1 {
                        return Err(Error::Protocol(format!(
                            "measurement row has {} fields, expected {}",
                            fields.len(),
                            value_columns + 1
                        )));
                    }
                    let freq = parse_f64(&fields[0])?;
                    if !freq.is_finite() || freq < 0.0 {
                        return Err(Error::Protocol(
                            "measurement frequency must be finite and nonnegative".into(),
                        ));
                    }
                    if points.last().is_some_and(|point| freq < point.freq_hz) {
                        return Err(Error::Protocol(
                            "measurement frequencies must not decrease".into(),
                        ));
                    }
                    // Measurement sentinels are format-dependent. Preserve
                    // parsed values until their device semantics are verified.
                    let values = fields[1..]
                        .iter()
                        .map(|f| parse_f64(f))
                        .collect::<Result<Vec<_>>>()?;
                    points.push(SweepPoint {
                        freq_hz: freq,
                        values,
                    });
                    progress(SweepProgress {
                        mode,
                        format: expected_format,
                        points: &points,
                        expected_points,
                    });
                }
                Some(StreamEvent::End { mode: m, .. }) if m == mode => {
                    if points.len() != expected_points as usize {
                        return Err(Error::Protocol(format!(
                            "measurement returned {} samples, expected {expected_points}",
                            points.len()
                        )));
                    }
                    if !packet.is_some_and(|packet| packet.name == expected_header) {
                        return Err(Error::Protocol(
                            "measurement ended without a complete packet".into(),
                        ));
                    }
                    return Ok(SweepData {
                        mode,
                        format: expected_format.to_string(),
                        points,
                    });
                }
                _ => {}
            }
        }
    }
}

impl<T: Transport> Drop for Device<T> {
    fn drop(&mut self) {
        // Never leave the instrument locked in remote mode (doc 1.2).
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Modulation, SourceAmplitude, SourceParams, SourcePort, SourceWarning};
    use std::collections::VecDeque;

    struct MockTransport {
        incoming: VecDeque<String>,
        sent: Vec<u8>,
        send_calls: usize,
        fail_send: Option<usize>,
        send_delay: Duration,
        read_delay: Duration,
        read_timeouts: Vec<Duration>,
        write_timeouts: Vec<Duration>,
        cancel_on_read: Option<(usize, CancellationToken)>,
        cancel_on_send: Option<(usize, CancellationToken)>,
    }

    impl MockTransport {
        fn with_lines(lines: &[&str]) -> Self {
            Self {
                incoming: lines.iter().map(|s| s.to_string()).collect(),
                sent: Vec::new(),
                send_calls: 0,
                fail_send: None,
                send_delay: Duration::ZERO,
                read_delay: Duration::ZERO,
                read_timeouts: Vec::new(),
                write_timeouts: Vec::new(),
                cancel_on_read: None,
                cancel_on_send: None,
            }
        }

        fn sent_text(&self) -> String {
            String::from_utf8_lossy(&self.sent).to_string()
        }

        fn queue_sweep(&mut self, mode: StreamMode, samples: u32) {
            self.incoming.push_back(match mode {
                StreamMode::S11 => "$start,s11,loss".into(),
                StreamMode::S21 => "$start,s21,loss".into(),
                StreamMode::Spec => "$start,spec".into(),
                _ => unreachable!("only implemented sweep modes are tested"),
            });
            for index in 0..samples {
                let frequency = 1_000_000 + u64::from(index) * 1_000_000 / u64::from(samples - 1);
                self.incoming.push_back(format!("${frequency},-12.5"));
            }
            self.incoming.push_back("$end".into());
        }

        fn queue_identity(&mut self) {
            self.incoming.extend(
                [
                    "$start,device",
                    "$<KC901>",
                    "$<-User @ :TEST>",
                    "$<-Software ver:V1.6.1-->",
                    "$<-Hardware ver:MB-V1.2-->",
                    "$<-Serial num:000000000001-->",
                    "$<-Copyright:KeXinShe-->",
                    "$end",
                ]
                .map(str::to_owned),
            );
        }
    }

    fn s11_params(points: u32) -> S11Params {
        S11Params {
            cal: Cal::CalOff,
            format: Format::Loss,
            points,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: None,
        }
    }

    fn spec_params(points: u32) -> SpecParams {
        SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        }
    }

    fn s21_params(points: u32) -> S21Params {
        S21Params {
            cal: Cal::CalOff,
            format: Format::Loss,
            lo: Lo::HighLo,
            points,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: None,
        }
    }

    fn point_params(mode: StreamMode) -> PointParams {
        PointParams {
            settings: match mode {
                StreamMode::S11 => PointSettings::S11 {
                    cal: Cal::CalOff,
                    format: Format::Loss,
                    rbw: Some(Rbw::R10k),
                },
                StreamMode::S21 => PointSettings::S21 {
                    cal: Cal::CalOff,
                    format: Format::Loss,
                    lo: Lo::HighLo,
                    rbw: Some(Rbw::R10k),
                },
                StreamMode::Spec => PointSettings::Spec {
                    cal: Cal::CalOff,
                    lo: Lo::HighLo,
                    rbw: Rbw::R10k,
                    ref_level_dbm: -10,
                },
                _ => unreachable!(),
            },
            frequency_hz: 1_000_000,
        }
    }

    fn queue_point(mock: &mut MockTransport, settings: &PointSettings, value: &str) {
        let header = if settings.format().is_empty() {
            format!("$start,{}", settings.mode().name())
        } else {
            format!("$start,{},{}", settings.mode().name(), settings.format())
        };
        mock.incoming
            .extend([header, format!("$999999,{value}"), "$end".into()]);
    }

    #[test]
    fn point_runs_use_continuous_count_one_then_fence_and_reinitialize() {
        for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
            let params = point_params(mode);
            let mut mock = MockTransport::with_lines(&[]);
            queue_point(&mut mock, &params.settings, "-12.5");
            queue_point(&mut mock, &params.settings, "999");
            mock.queue_identity();
            queue_point(&mut mock, &params.settings, "-20");
            mock.queue_identity();
            let mut dev = Device::new(mock);
            for expected in [-12.5, -20.0] {
                let point = dev.measure_point(&params).unwrap();
                assert_eq!(point.mode, mode);
                assert_eq!(point.format, params.settings.format());
                assert_eq!(point.points.len(), 1);
                assert_eq!(point.points[0].freq_hz, 999_999.0);
                assert_eq!(point.points[0].values, [expected]);
                assert!(!dev.requires_reconnect());
                assert_eq!(dev.active_mode, None);
                assert_eq!(dev.last_rbw, None);
            }
            let (receiver, run) = match mode {
                StreamMode::S11 => ("$bw,10k\n", "$s11,run,caloff,loss,1,ss,1000000\n"),
                StreamMode::S21 => ("$bw,10k\n", "$s21,run,caloff,loss,highlo,1,ss,1000000\n"),
                StreamMode::Spec => (
                    "$bw,10k\n$specref,-10\n",
                    "$spec,run,caloff,highlo,1,ss,1000000\n",
                ),
                _ => unreachable!(),
            };
            let commands = format!(
                "$s11,stop\n$s21,stop\n$spec,stop\n${},init\n{receiver}{run}\x03$device\n",
                mode.name()
            );
            assert_eq!(dev.transport.sent_text(), commands.repeat(2));
            assert!(dev.transport.incoming.is_empty());
        }
    }

    #[test]
    fn point_repeated_headers_replace_incomplete_rows_without_relabeling() {
        let params = point_params(StreamMode::S11);
        let mut mock = MockTransport::with_lines(&[
            "$start,s11,loss",
            "$1000000,999",
            "$start,s11,loss",
            "$start,s11,loss",
            "$1000000.25,NaN",
            "$end",
        ]);
        mock.queue_identity();
        let mut dev = Device::new(mock);
        let point = dev.measure_point(&params).unwrap();
        assert_eq!(point.points.len(), 1);
        assert_eq!(point.points[0].freq_hz, 1_000_000.25);
        assert!(point.points[0].values[0].is_nan());
    }

    #[test]
    fn point_invalid_parameters_and_retired_sessions_perform_no_io() {
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        let mut params = point_params(StreamMode::S11);
        params.frequency_hz = 0;
        assert!(matches!(
            dev.measure_point(&params),
            Err(Error::InvalidParameter(_))
        ));
        params.frequency_hz = 1_000_000;
        params.settings = PointSettings::S11 {
            cal: Cal::CalOn,
            format: Format::Delay,
            rbw: Some(Rbw::R100Hz),
        };
        assert!(matches!(
            dev.measure_point(&params),
            Err(Error::InvalidParameter(_))
        ));
        dev.session_failed = true;
        assert!(matches!(
            dev.measure_point(&point_params(StreamMode::S11)),
            Err(Error::NotConnected)
        ));
        assert!(dev.transport.sent.is_empty());
        assert!(dev.transport.read_timeouts.is_empty());
    }

    #[test]
    fn malformed_point_frames_are_never_published_or_reused_after_cleanup() {
        for lines in [
            vec!["$start,s11,loss", "$end"],
            vec!["$start,s11,loss", "$1000000,1", "$1000000,2", "$end"],
            vec!["$start,s11,loss", "$1000000,1,2", "$end"],
            vec!["$start,s11,loss", "$NaN,1", "$end"],
            vec!["$start,s11,loss", "$-1,1", "$end"],
            vec!["$start,s21,loss", "$1000000,1", "$end"],
            vec!["$start,s11,z", "$1000000,1,2,3", "$end"],
        ] {
            let mut mock = MockTransport::with_lines(&lines);
            mock.queue_identity();
            let mut dev = Device::new(mock);
            let params = point_params(StreamMode::S11);
            assert!(
                matches!(dev.measure_point(&params), Err(Error::Protocol(_))),
                "{lines:?}"
            );
            assert!(dev.requires_reconnect());
            assert!(dev.transport.sent.ends_with(b"\x03$device\n"));
            let sent = dev.transport.sent.clone();
            assert!(matches!(
                dev.measure_point(&params),
                Err(Error::NotConnected)
            ));
            assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn point_failure_fence_retires_complete_but_unaccepted_data() {
        for failure in ["identity", "device", "abort", "query"] {
            let params = point_params(StreamMode::S11);
            let mut mock = MockTransport::with_lines(&[]);
            queue_point(&mut mock, &params.settings, "1");
            match failure {
                "identity" => mock
                    .incoming
                    .extend(["$start,device", "$bad", "$end"].map(str::to_owned)),
                "device" => mock
                    .incoming
                    .extend(["$start,err_cmd", "$bad", "$end"].map(str::to_owned)),
                "abort" => mock.fail_send = Some(7),
                "query" => mock.fail_send = Some(8),
                _ => unreachable!(),
            }
            let mut dev = Device::new(mock);
            assert!(dev.measure_point(&params).is_err(), "{failure}");
            assert!(dev.requires_reconnect(), "{failure}");
            let sent = dev.transport.sent.clone();
            assert!(matches!(
                dev.measure_point(&params),
                Err(Error::NotConnected)
            ));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn point_cancellation_discards_data_even_during_the_final_identity_fence() {
        for cancel_read in [1, 2, 3, 4, 11] {
            let params = point_params(StreamMode::S11);
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            queue_point(&mut mock, &params.settings, "1");
            mock.queue_identity();
            mock.cancel_on_read = Some((cancel_read, cancel.clone()));
            let mut dev = Device::new(mock);
            assert!(
                matches!(
                    dev.measure_point_controlled(&params, &cancel),
                    Err(Error::Cancelled)
                ),
                "read {cancel_read}"
            );
            assert!(!dev.requires_reconnect());
            assert!(dev.transport.sent.ends_with(b"\x03$device\n"));
            assert!(dev.transport.incoming.is_empty());
        }
    }

    #[test]
    fn point_setup_cancellation_never_sends_run_and_still_synchronizes() {
        for send in 1..=5 {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.cancel_on_send = Some((send, cancel.clone()));
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.measure_point_controlled(&point_params(StreamMode::S11), &cancel),
                Err(Error::Cancelled)
            ));
            assert!(!dev.transport.sent_text().contains(",run,"));
            assert!(dev.transport.sent.ends_with(b"\x03$device\n"));
            assert!(!dev.requires_reconnect());
        }
    }

    #[test]
    fn framed_point_device_error_is_recoverable_only_after_a_successful_fence() {
        let params = point_params(StreamMode::S11);
        let mut mock = MockTransport::with_lines(&["$start,err_par5", "$error", "$end"]);
        mock.queue_identity();
        queue_point(&mut mock, &params.settings, "2");
        mock.queue_identity();
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.measure_point(&params), Err(Error::Device(name)) if name == "err_par5")
        );
        assert!(!dev.requires_reconnect());
        assert_eq!(dev.measure_point(&params).unwrap().points[0].values, [2.0]);
    }

    #[test]
    fn point_device_error_preserves_its_diagnostic_when_cleanup_also_fails() {
        let params = point_params(StreamMode::S11);
        let mut mock = MockTransport::with_lines(&["$start,err_par5", "$error", "$end"]);
        mock.fail_send = Some(7);
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.measure_point(&params), Err(Error::Device(name)) if name == "err_par5")
        );
        assert!(dev.requires_reconnect());
        let sent = dev.transport.sent.clone();
        assert!(matches!(
            dev.measure_point(&params),
            Err(Error::NotConnected)
        ));
        assert_eq!(dev.transport.sent, sent);
    }

    impl Transport for MockTransport {
        fn send_with_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
            self.send_calls += 1;
            self.write_timeouts.push(timeout);
            let started = Instant::now();
            if !self.send_delay.is_zero() {
                sleep(self.send_delay);
            }
            remaining_timeout(started, timeout)?;
            if self.fail_send == Some(self.send_calls) {
                return Err(Error::NotConnected);
            }
            self.sent.extend_from_slice(data);
            if let Some((call, cancel)) = &self.cancel_on_send
                && *call == self.send_calls
            {
                cancel.cancel();
            }
            Ok(())
        }

        fn recv_line(&mut self, timeout: Duration) -> Result<String> {
            self.read_timeouts.push(timeout);
            if let Some((call, cancel)) = &self.cancel_on_read
                && *call == self.read_timeouts.len()
            {
                cancel.cancel();
            }
            if !self.read_delay.is_zero() {
                sleep(self.read_delay);
            }
            match self.incoming.pop_front() {
                Some(line) => Ok(line),
                None => {
                    sleep(timeout);
                    Err(Error::Timeout)
                }
            }
        }
    }

    #[test]
    fn pre_cancelled_operations_do_not_touch_the_session() {
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(matches!(
            dev.handshake_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.device_info_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.temperature_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.voltage_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.sweep_s11_controlled(&s11_params(3), &cancel, |_| panic!("no preview expected")),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.sweep_spec_controlled(&spec_params(3), &cancel, |_| panic!("no preview expected")),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.sweep_s21_controlled(&s21_params(3), &cancel, |_| panic!("no preview expected")),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.measure_point_controlled(&point_params(StreamMode::S11), &cancel),
            Err(Error::Cancelled)
        ));
        assert!(dev.transport.sent.is_empty());
        assert!(dev.transport.read_timeouts.is_empty());
        assert!(!dev.requires_reconnect());
    }

    #[test]
    fn cancelled_sweeps_discard_tails_and_require_a_fresh_identity_boundary() {
        for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
            for cancel_after in [1, 3] {
                let mut mock = MockTransport::with_lines(&[]);
                mock.queue_sweep(mode, 3);
                mock.queue_identity();
                mock.queue_sweep(mode, 3);
                let mut dev = Device::new(mock);
                let cancel = CancellationToken::default();
                let mut lengths = Vec::new();
                let progress = |prefix: SweepProgress<'_>| {
                    lengths.push(prefix.points.len());
                    assert_eq!(prefix.mode, mode);
                    assert_eq!(prefix.expected_points, 3);
                    if prefix.points.len() == cancel_after {
                        cancel.cancel();
                    }
                };
                let result = match mode {
                    StreamMode::S11 => dev.sweep_s11_controlled(&s11_params(3), &cancel, progress),
                    StreamMode::S21 => dev.sweep_s21_controlled(&s21_params(3), &cancel, progress),
                    StreamMode::Spec => {
                        dev.sweep_spec_controlled(&spec_params(3), &cancel, progress)
                    }
                    _ => unreachable!(),
                };
                assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
                assert_eq!(lengths, (0..=cancel_after).collect::<Vec<_>>());
                assert!(dev.transport.sent.ends_with(b"\x03$device\n"));
                assert_eq!(dev.active_mode, None);
                assert_eq!(dev.last_rbw, None);
                assert!(!dev.requires_reconnect());
                let data = match mode {
                    StreamMode::S11 => dev.sweep_s11(&s11_params(3)),
                    StreamMode::S21 => dev.sweep_s21(&s21_params(3)),
                    StreamMode::Spec => dev.sweep_spec(&spec_params(3)),
                    _ => unreachable!(),
                }
                .unwrap();
                assert_eq!(data.points.len(), 3);
            }
        }
    }

    #[test]
    fn cancellation_during_setup_does_not_start_a_sweep() {
        for send in 1..=4 {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.cancel_on_send = Some((send, cancel.clone()));
            mock.queue_identity();
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.sweep_s11_controlled(&s11_params(3), &cancel, |_| {}),
                Err(Error::Cancelled)
            ));
            assert!(!dev.transport.sent_text().contains(",run,"));
            assert!(dev.transport.sent.ends_with(b"\x03$device\n"));
            assert!(!dev.requires_reconnect());
        }
    }

    #[test]
    fn an_end_marker_or_malformed_identity_does_not_confirm_cancellation() {
        for lines in [
            vec!["$start,s11,loss", "$5000,1", "$end"],
            vec!["$start,device", "$incomplete identity", "$end"],
            vec!["$start,err_cmd", "$error", "$end"],
        ] {
            let mut dev = Device::new(MockTransport::with_lines(&lines));
            assert!(dev.cancel_sweep(Duration::from_millis(25)).is_err());
            assert!(dev.requires_reconnect());
            let sent = dev.transport.sent.clone();
            assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn cancellation_cleanup_write_failure_retires_the_session() {
        for send in 1..=2 {
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.fail_send = Some(send);
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.cancel_sweep(CLEANUP_TIMEOUT),
                Err(Error::NotConnected)
            ));
            assert!(dev.requires_reconnect());
            assert_eq!(dev.transport.send_calls, send);
        }
    }

    #[test]
    fn interrupted_queries_retire_pending_replies() {
        let cancel = CancellationToken::default();
        let mut mock = MockTransport::with_lines(&["$start,temp", "$47.3", "$end"]);
        mock.cancel_on_read = Some((1, cancel.clone()));
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.temperature_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(dev.requires_reconnect());
        assert_eq!(dev.transport.incoming.len(), 2);
        assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
    }

    #[test]
    fn cancelled_handshake_is_released_but_refused_handshake_is_not() {
        let cancel = CancellationToken::default();
        let mut mock = MockTransport::with_lines(&["[KC901]000000000001"]);
        mock.cancel_on_read = Some((1, cancel.clone()));
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.handshake_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert!(dev.requires_reconnect());
        assert!(!dev.remote);
        dev.close();
        assert_eq!(dev.transport.sent_text(), "C$local\n");
        let mut dev = Device::new(MockTransport::with_lines(&[
            "$start,ConFail",
            "$busy",
            "$end",
        ]));
        assert!(matches!(dev.handshake(), Err(Error::DeviceBusy(_))));
        dev.close();
        assert_eq!(dev.transport.sent, b"C");
    }

    #[test]
    fn cancellation_does_not_discard_a_fully_received_handshake_refusal() {
        let cancel = CancellationToken::default();
        let mut mock = MockTransport::with_lines(&["$start,ConFail", "$busy", "$end"]);
        mock.cancel_on_read = Some((3, cancel.clone()));
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.handshake_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        dev.close();
        assert_eq!(dev.transport.sent, b"C");
    }

    #[test]
    fn closing_is_terminal_even_when_the_release_write_fails() {
        for fail_send in [None, Some(1)] {
            let mut dev = Device::new(MockTransport::with_lines(&[]));
            dev.remote = true;
            dev.transport.fail_send = fail_send;
            dev.close();
            assert!(dev.requires_reconnect());
            let sent = dev.transport.sent.clone();
            assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
            assert!(matches!(dev.handshake(), Err(Error::NotConnected)));
            assert!(matches!(
                dev.sweep_s11(&s11_params(3)),
                Err(Error::NotConnected)
            ));
            assert_eq!(dev.transport.sent, sent);
            assert!(dev.transport.read_timeouts.is_empty());
        }
    }

    #[test]
    fn close_writes_share_one_cleanup_budget() {
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        dev.remote = true;
        dev.active_mode = Some(StreamMode::S11);
        dev.transport.send_delay = Duration::from_millis(3);
        dev.close();
        assert_eq!(dev.transport.write_timeouts.len(), 2);
        assert!(!dev.transport.write_timeouts[0].is_zero());
        assert!(dev.transport.write_timeouts[0] <= CLEANUP_TIMEOUT);
        assert!(dev.transport.write_timeouts[1] < CLEANUP_TIMEOUT);
        assert_eq!(dev.transport.sent_text(), "$s11,stop\n$local\n");
    }

    #[test]
    fn preview_restarts_replace_the_previous_prefix() {
        let mock = MockTransport::with_lines(&[
            "$start,s11,loss",
            "$5000,999",
            "$start,s11,loss",
            "$5000,1",
            "$6000,2",
            "$7000,3",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let mut prefixes = Vec::new();
        let data = dev
            .sweep_s11_controlled(&s11_params(3), &CancellationToken::default(), |prefix| {
                prefixes.push(
                    prefix
                        .points
                        .iter()
                        .map(|point| point.values[0])
                        .collect::<Vec<_>>(),
                );
            })
            .unwrap();
        assert_eq!(
            prefixes,
            [
                vec![],
                vec![999.0],
                vec![],
                vec![1.0],
                vec![1.0, 2.0],
                vec![1.0, 2.0, 3.0]
            ]
        );
        assert_eq!(data.points.len(), 3);
    }

    #[test]
    fn handshake_accepts_verified_id_packet() {
        let mock = MockTransport::with_lines(&["$start,id", "$000000000001", "$end"]);
        let mut dev = Device::new(mock);
        assert_eq!(dev.handshake().unwrap(), "000000000001");
        assert_eq!(dev.transport.sent, b"C");
        dev.close();
        assert_eq!(dev.transport.sent_text(), "C$local\n");
    }

    #[test]
    fn handshake_tolerates_manual_kc901_form() {
        let mock = MockTransport::with_lines(&["[KC901]002015123456"]);
        let mut dev = Device::new(mock);
        assert_eq!(dev.handshake().unwrap(), "002015123456");
    }

    #[test]
    fn handshake_confail_maps_to_device_busy() {
        let mock = MockTransport::with_lines(&[
            "$start,ConFail",
            "$Please exit the window operation first.",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        assert!(matches!(dev.handshake(), Err(Error::DeviceBusy(_))));
    }

    #[test]
    fn info_queries_parse_packets() {
        let mock = MockTransport::with_lines(&[
            "$start,device",
            "$<----------------------KC901 network analyzer----------------->",
            "$<-User @ :VOWSTAR>",
            "$<-Software ver:V1.6.1------------------------------------------>",
            "$<-Hardware ver:MB-V1.2 RB-V1.0.5------------------------------->",
            "$<-Serial num:000000000001-------------------------------------->",
            "$<-Copyright:KeXinShe Co.,Ltd & KeChuang measurement association>",
            "$<-------------------------------------------------------------->",
            "$end",
            "$start,temp",
            "$47.3",
            "$end",
            "$start,voltage",
            "$12.16,8.03",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let info = dev.device_info().unwrap();
        assert_eq!(info.software, "V1.6.1");
        assert_eq!(info.serial, "000000000001");
        assert_eq!(dev.temperature().unwrap(), 47.3);
        let v = dev.voltage().unwrap();
        assert_eq!((v.external, v.battery), (12.16, 8.03));
        assert_eq!(dev.transport.sent_text(), "$device\n$temp\n$voltage\n");
    }

    #[test]
    fn err_packet_aborts_query() {
        let mock =
            MockTransport::with_lines(&["$start,err_cmd", "$error:Command input error!", "$end"]);
        let mut dev = Device::new(mock);
        assert!(matches!(dev.temperature(), Err(Error::Device(ref n)) if n == "err_cmd"));
    }

    #[test]
    fn handshake_budget_is_not_reset_by_unrelated_lines() {
        let mut mock = MockTransport::with_lines(&["noise"; 100]);
        mock.read_delay = Duration::from_millis(3);
        let mut dev = Device::new(mock);
        let budget = Duration::from_millis(25);
        assert!(matches!(
            dev.handshake_with_timeout(budget),
            Err(Error::Timeout)
        ));
        assert!(!dev.transport.incoming.is_empty());
        assert!(
            dev.transport
                .read_timeouts
                .windows(2)
                .all(|pair| pair[1] < pair[0])
        );
        assert!(!dev.remote);
    }

    #[test]
    fn query_budget_is_not_reset_by_unrelated_packets() {
        let mut mock = MockTransport::with_lines(&[]);
        for _ in 0..100 {
            mock.incoming.extend(["$start,other".into(), "$end".into()]);
        }
        mock.read_delay = Duration::from_millis(3);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.query_packet(commands::TEMP, "temp", Duration::from_millis(25)),
            Err(Error::Timeout)
        ));
        assert!(!dev.transport.incoming.is_empty());
        assert!(
            dev.transport
                .read_timeouts
                .windows(2)
                .all(|pair| pair[1] < pair[0])
        );
    }

    #[test]
    fn query_and_handshake_budgets_include_send_time() {
        for handshake in [false, true] {
            let mut mock = MockTransport::with_lines(&["$start,temp", "$47.3", "$end"]);
            mock.send_delay = Duration::from_millis(20);
            let mut dev = Device::new(mock);
            let budget = Duration::from_millis(5);
            let result = if handshake {
                dev.handshake_with_timeout(budget).map(|_| ())
            } else {
                dev.query_packet(commands::TEMP, "temp", budget).map(|_| ())
            };
            assert!(matches!(result, Err(Error::Timeout)));
            assert!(dev.transport.read_timeouts.is_empty());
        }
    }

    #[test]
    fn reply_arriving_after_budget_is_not_accepted() {
        let mut mock = MockTransport::with_lines(&["[KC901]000000000001"]);
        mock.read_delay = Duration::from_millis(20);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.handshake_with_timeout(Duration::from_millis(5)),
            Err(Error::Timeout)
        ));
        assert!(!dev.remote);

        let mut mock = MockTransport::with_lines(&["$end"]);
        mock.read_delay = Duration::from_millis(20);
        let mut dev = Device::new(mock);
        dev.packets.feed_line("$start,temp").unwrap();
        dev.packets.feed_line("$47.3").unwrap();
        assert!(matches!(
            dev.expect_packet("temp", Instant::now(), Duration::from_millis(5)),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn maximum_timeout_does_not_overflow_an_instant() {
        assert!(remaining_timeout(Instant::now(), Duration::MAX).is_ok());
        let mock = MockTransport::with_lines(&["$start,temp", "$47.3", "$end"]);
        let mut dev = Device::new(mock);
        let packet = dev
            .query_packet(commands::TEMP, "temp", Duration::MAX)
            .unwrap();
        assert_eq!(packet.name, "temp");
        assert!(
            dev.transport
                .read_timeouts
                .iter()
                .all(|&timeout| timeout == POLL_INTERVAL)
        );
    }

    fn collect_s11(lines: &[&str], format: Format, points: u32) -> Result<SweepData> {
        Device::new(MockTransport::with_lines(lines)).collect_stream(
            StreamMode::S11,
            Some(format),
            points,
            Duration::from_millis(100),
        )
    }

    #[test]
    fn complete_streams_require_the_requested_sample_count() {
        for samples in [0, 1, 2, 4] {
            let mut mock = MockTransport::with_lines(&[]);
            mock.incoming.push_back("$start,s11,loss".into());
            for index in 0..samples {
                mock.incoming.push_back(format!("${},1", 5000 + index));
            }
            mock.incoming.push_back("$end".into());
            let mut dev = Device::new(mock);
            let result =
                dev.collect_stream(StreamMode::S11, Some(Format::Loss), 3, GENERIC_TIMEOUT);
            assert!(matches!(result, Err(Error::Protocol(_))), "{samples}");
            if samples > 3 {
                assert_eq!(dev.transport.incoming.front().unwrap(), "$end");
            }
        }
    }

    #[test]
    fn streams_require_the_requested_mode_and_exact_format_header() {
        for header in [
            "$start,spec",
            "$start,s21,loss",
            "$start,s11,ma",
            "$start,s11,loss,extra",
        ] {
            let result = collect_s11(
                &[header, "$5000,1", "$6000,2", "$7000,3", "$end"],
                Format::Loss,
                3,
            );
            assert!(matches!(result, Err(Error::Protocol(_))), "{header}");
        }
        let mut dev = Device::new(MockTransport::with_lines(&[
            "$start,spec,loss",
            "$5000,-80",
            "$6000,-81",
            "$7000,-82",
            "$end",
        ]));
        assert!(matches!(
            dev.collect_stream(StreamMode::Spec, None, 3, GENERIC_TIMEOUT),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn every_s11_format_requires_its_exact_value_column_count() {
        for (format, columns) in [
            (Format::Ri, 2),
            (Format::Ma, 2),
            (Format::Vswr, 1),
            (Format::Loss, 1),
            (Format::Z, 3),
        ] {
            for actual in [columns - 1, columns, columns + 1] {
                let header = format!("$start,s11,{format}");
                let row = format!("$5000{}", ",1".repeat(actual));
                let result = collect_s11(&[&header, &row, "$end"], format, 1);
                assert_eq!(result.is_ok(), actual == columns, "{format}: {actual}");
            }
        }
        for row in ["$5000", "$5000,-80,-81"] {
            let mock = MockTransport::with_lines(&["$start,spec", row, "$end"]);
            assert!(matches!(
                Device::new(mock).collect_stream(StreamMode::Spec, None, 1, GENERIC_TIMEOUT),
                Err(Error::Protocol(_))
            ));
        }
    }

    #[test]
    fn invalid_reported_frequencies_are_rejected() {
        for frequency in ["NaN", "inf", "-inf", "-1", "not-a-number"] {
            let row = format!("${frequency},1");
            assert!(matches!(
                collect_s11(&["$start,s11,loss", &row, "$end"], Format::Loss, 1),
                Err(Error::Protocol(_))
            ));
        }
        assert!(matches!(
            collect_s11(
                &["$start,s11,loss", "$6000,1", "$5000,2", "$end"],
                Format::Loss,
                2,
            ),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn reported_frequency_rounding_and_measurement_sentinels_are_preserved() {
        // Repeated frequencies and nonfinite values are synthetic cases, not
        // an assertion about the firmware's precision or sentinel conventions.
        let data = collect_s11(
            &[
                "$start,s11,loss",
                "$6999999200,NaN",
                "$6999999200,inf",
                "$7000000200,-inf",
                "$end",
            ],
            Format::Loss,
            3,
        )
        .unwrap();
        assert_eq!(data.points[0].freq_hz, 6_999_999_200.0);
        assert_eq!(data.points[1].freq_hz, 6_999_999_200.0);
        assert_eq!(data.points[2].freq_hz, 7_000_000_200.0);
        assert!(data.points[0].values[0].is_nan());
        assert_eq!(data.points[1].values[0], f64::INFINITY);
        assert_eq!(data.points[2].values[0], f64::NEG_INFINITY);
        let mock = MockTransport::with_lines(&["$start,spec", "$0,-80", "$end"]);
        assert_eq!(
            Device::new(mock)
                .collect_stream(StreamMode::Spec, None, 1, GENERIC_TIMEOUT)
                .unwrap()
                .points[0]
                .freq_hz,
            0.0
        );
    }

    #[test]
    fn missing_stream_end_and_unrelated_lines_cannot_extend_the_budget() {
        let mut mock = MockTransport::with_lines(&["noise"; 100]);
        mock.read_delay = Duration::from_millis(3);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.collect_stream(
                StreamMode::S11,
                Some(Format::Loss),
                3,
                Duration::from_millis(25),
            ),
            Err(Error::Timeout)
        ));
        assert!(!dev.transport.incoming.is_empty());
        assert!(matches!(
            collect_s11(&["$start,s11,loss", "$5000,1"], Format::Loss, 1),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn restarted_stream_does_not_include_incomplete_frame_points() {
        let mock = MockTransport::with_lines(&[
            "$start,s11,loss",
            "$5000,999",
            "$start,s11,loss",
            "$1000000,1",
            "$1500000,2",
            "$2000000,3",
            "$end",
        ]);
        let data = Device::new(mock).sweep_s11(&s11_params(3)).unwrap();
        assert_eq!(data.points.len(), 3);
        assert_eq!(data.points[0].freq_hz, 1_000_000.0);
        assert_eq!(data.points[0].values, [1.0]);
    }

    #[test]
    fn device_error_inside_an_incomplete_stream_is_reported() {
        let mock = MockTransport::with_lines(&[
            "$start,s11,loss",
            "$1000000,1",
            "$start,err_par5",
            "$invalid frequency",
            "$end",
        ]);
        let error = Device::new(mock).sweep_s11(&s11_params(3)).unwrap_err();
        assert!(matches!(error, Error::Device(name) if name == "err_par5"));
    }

    #[test]
    fn sweep_s11_consumes_manual_example_stream() {
        let mock = MockTransport::with_lines(&[
            "$start,s11,ri",
            "$75000000,0.528e0,-0.269e0",
            "$100000000,0.471e0,-0.406e0",
            "$125000000,0.370e0,-0.475e0",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = S11Params {
            cal: Cal::CalOff,
            format: Format::Ri,
            points: 3,
            start_hz: 75_000_000,
            stop_hz: 125_000_000,
            rbw: None,
        };
        let data = dev.sweep_s11(&params).unwrap();
        assert_eq!(data.mode, StreamMode::S11);
        assert_eq!(data.format, "ri");
        assert_eq!(data.points.len(), 3);
        assert_eq!(data.points[0].freq_hz, 75_000_000.0);
        assert_eq!(data.points[0].values, vec![0.528, -0.269]);
        assert_eq!(
            dev.transport.sent_text(),
            "$s11,stop\n$s21,stop\n$spec,stop\n$s11,init\n$s11,run,caloff,ri,2,ss,75000000,125000000\n"
        );
    }

    #[test]
    fn sweep_s11_with_rbw_pushes_bw_command() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        let params = S11Params {
            cal: Cal::CalSys,
            rbw: Some(Rbw::R10k),
            ..s11_params(3)
        };
        let data = dev.sweep_s11(&params).unwrap();
        assert_eq!(data.points[0].values, vec![-12.5]);
        assert!(dev.transport.sent_text().contains("$bw,10k\n"));
    }

    #[test]
    fn sweep_spec_sends_full_sequence() {
        let mock = MockTransport::with_lines(&[
            "$start,spec",
            "$75000000,-74.166",
            "$87500000,-73.002",
            "$100000000,-71.002",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 3,
            start_hz: 75_000_000,
            stop_hz: 100_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        };
        let data = dev.sweep_spec(&params).unwrap();
        assert_eq!(data.mode, StreamMode::Spec);
        assert_eq!(data.format, "");
        assert_eq!(data.points.len(), 3);
        assert_eq!(data.points[2].values, vec![-71.002]);
        assert_eq!(
            dev.transport.sent_text(),
            "$s11,stop\n$s21,stop\n$spec,stop\n$spec,init\n$bw,10k\n$specref,-10\n$spec,run,caloff,highlo,2,ss,75000000,100000000\n"
        );
    }

    #[test]
    fn s21_formats_keep_raw_columns_units_and_exact_commands() {
        for (format, row, values) in [
            (Format::Ri, "-0.5,0.25", vec![-0.5, 0.25]),
            (Format::Ma, "0.75,-90", vec![0.75, -90.0]),
            (Format::Loss, "-3.25", vec![-3.25]),
            (Format::Delay, "-5.147039e-09", vec![-5.147039e-9]),
        ] {
            for lo in [Lo::HighLo, Lo::LowLo] {
                let header = format!("$start,s21,{format}");
                let rows = [
                    format!("$0,{row}"),
                    format!("$500,{row}"),
                    format!("$1000,{row}"),
                ];
                let mock =
                    MockTransport::with_lines(&[&header, &rows[0], &rows[1], &rows[2], "$end"]);
                let mut dev = Device::new(mock);
                let params = S21Params {
                    format,
                    lo,
                    start_hz: 0,
                    stop_hz: 1000,
                    rbw: Some(Rbw::R1k),
                    ..s21_params(3)
                };
                let mut prefixes = Vec::new();
                let data = dev
                    .sweep_s21_controlled(&params, &CancellationToken::default(), |prefix| {
                        assert_eq!(prefix.mode, StreamMode::S21);
                        assert_eq!(prefix.format, format.as_str());
                        prefixes.push(prefix.points.len());
                    })
                    .unwrap();
                assert_eq!(prefixes, [0, 1, 2, 3]);
                assert_eq!(data.mode, StreamMode::S21);
                assert_eq!(data.format, format.as_str());
                assert_eq!(data.points[0].freq_hz, 0.0);
                assert!(data.points.iter().all(|point| point.values == values));
                assert_eq!(
                    dev.transport.sent_text(),
                    format!(
                        "$s11,stop\n$s21,stop\n$spec,stop\n$s21,init\n$bw,1k\n$s21,run,caloff,{format},{lo},2,ss,0,1000\n"
                    )
                );
            }
        }
    }

    #[test]
    fn s21_schema_mismatch_cannot_be_reused_as_a_later_sweep() {
        for (format, header, row, count) in [
            (Format::Ri, "$start,s21,ri", "0.5", 3),
            (Format::Ma, "$start,s21,ma", "0.5,1,2", 3),
            (Format::Delay, "$start,s21,delay", "1e-9,2e-9", 3),
            (Format::Loss, "$start,s21,loss", "1", 2),
            (Format::Loss, "$start,s21,loss", "1", 4),
            (Format::Loss, "$start,s11,loss", "1", 3),
            (Format::Loss, "$start,s21,delay", "1", 3),
            (Format::Loss, "$start,s21,loss,extra", "1", 3),
        ] {
            let mut mock = MockTransport::with_lines(&[header]);
            for index in 0..count {
                mock.incoming
                    .push_back(format!("${},{row}", 1_000_000 + index * 1000));
            }
            mock.incoming.push_back("$end".into());
            mock.queue_sweep(StreamMode::S21, 3);
            let mut dev = Device::new(mock);
            let params = S21Params {
                format,
                ..s21_params(3)
            };
            assert!(
                matches!(dev.sweep_s21(&params), Err(Error::Protocol(_))),
                "{header} {row} {count}"
            );
            assert!(dev.requires_reconnect());
            let sent = dev.transport.sent.clone();
            assert!(matches!(dev.sweep_s21(&params), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn invalid_s21_requests_do_not_change_an_initialized_mode() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        dev.sweep_s11(&s11_params(3)).unwrap();
        let sent = dev.transport.sent.clone();
        for params in [
            S21Params {
                start_hz: 2_000_000,
                ..s21_params(3)
            },
            S21Params {
                stop_hz: 7_000_000_001,
                ..s21_params(3)
            },
            S21Params {
                cal: Cal::CalOn,
                ..s21_params(3)
            },
            S21Params {
                format: Format::Z,
                ..s21_params(3)
            },
            S21Params {
                format: Format::Vswr,
                ..s21_params(3)
            },
            S21Params {
                rbw: Some(Rbw::R100Hz),
                ..s21_params(3)
            },
            s21_params(2),
            s21_params(1002),
        ] {
            assert!(matches!(
                dev.sweep_s21(&params),
                Err(Error::InvalidParameter(_))
            ));
            assert_eq!(dev.transport.sent, sent);
            assert_eq!(dev.active_mode, Some(StreamMode::S11));
            assert!(!dev.requires_reconnect());
        }
    }

    #[test]
    fn s21_repeats_switches_bandwidth_and_sample_count_conversion() {
        for (samples, wire) in [(3, 2), (201, 200), (1001, 1000)] {
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_sweep(StreamMode::S11, 3);
            mock.queue_sweep(StreamMode::S21, samples);
            mock.queue_sweep(StreamMode::S21, samples);
            mock.queue_sweep(StreamMode::Spec, 3);
            let mut dev = Device::new(mock);
            dev.sweep_s11(&S11Params {
                rbw: Some(Rbw::R1k),
                ..s11_params(3)
            })
            .unwrap();
            let before_s21 = dev.transport.sent.len();
            dev.sweep_s21(&s21_params(samples)).unwrap();
            assert_eq!(
                &dev.transport.sent_text()[before_s21..],
                format!(
                    "$s11,stop\n$s21,init\n$s21,run,caloff,loss,highlo,{wire},ss,1000000,2000000\n"
                )
            );
            assert_eq!(dev.last_rbw, Some(Rbw::R1k));
            assert_eq!(
                dev.sweep_timeout(samples),
                model::sweep_timeout(Some(Rbw::R1k), samples)
            );
            let before_repeat = dev.transport.sent.len();
            assert_eq!(
                dev.sweep_s21(&s21_params(samples)).unwrap().points.len(),
                samples as usize
            );
            assert_eq!(
                &dev.transport.sent_text()[before_repeat..],
                format!("$s21,run,caloff,loss,highlo,{wire},ss,1000000,2000000\n")
            );
            let before_spec = dev.transport.sent.len();
            dev.sweep_spec(&spec_params(3)).unwrap();
            assert!(dev.transport.sent[before_spec..].starts_with(b"$s21,stop\n$spec,init\n"));
        }
    }

    #[test]
    fn s21_setup_and_run_write_failure_retires_the_session() {
        for failed_send in 1..=6 {
            let mut mock = MockTransport::with_lines(&[]);
            mock.fail_send = Some(failed_send);
            let mut dev = Device::new(mock);
            let params = S21Params {
                rbw: Some(Rbw::R1k),
                ..s21_params(3)
            };
            assert!(matches!(dev.sweep_s21(&params), Err(Error::NotConnected)));
            assert!(dev.requires_reconnect());
            let sent = dev.transport.sent.clone();
            assert!(matches!(dev.sweep_s21(&params), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn s21_restart_preserves_reported_rounding_and_raw_measurement_values() {
        // Synthetic edge values exercise the shared parser's preservation
        // policy. They are not a claim about S21 firmware sentinel values.
        let mock = MockTransport::with_lines(&[
            "$start,s21,delay",
            "$6999999000,999",
            "$start,s21,delay",
            "$6999999000,NaN",
            "$6999999000,inf",
            "$7000000200,-5e-9",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = S21Params {
            format: Format::Delay,
            start_hz: 6_999_999_000,
            stop_hz: 7_000_000_000,
            ..s21_params(3)
        };
        let mut prefixes = Vec::new();
        let data = dev
            .sweep_s21_controlled(&params, &CancellationToken::default(), |prefix| {
                prefixes.push(prefix.points.len());
            })
            .unwrap();
        assert_eq!(prefixes, [0, 1, 0, 1, 2, 3]);
        assert_eq!(data.points[0].freq_hz, data.points[1].freq_hz);
        assert_eq!(data.points[2].freq_hz, 7_000_000_200.0);
        assert!(data.points[0].values[0].is_nan());
        assert_eq!(data.points[1].values, [f64::INFINITY]);
        assert_eq!(data.points[2].values, [-5e-9]);
    }

    #[test]
    fn a_framed_s21_device_error_is_diagnosable_and_allows_reinitialization() {
        let mut mock = MockTransport::with_lines(&[
            "$start,s21,loss",
            "$1000000,-10",
            "$start,err_uninit",
            "$invalid mode state",
            "$end",
        ]);
        mock.queue_sweep(StreamMode::S21, 3);
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.sweep_s21(&s21_params(3)), Err(Error::Device(name)) if name == "err_uninit")
        );
        assert!(!dev.requires_reconnect());
        assert!(dev.active_mode.is_none());
        let before_retry = dev.transport.sent.len();
        let data = dev.sweep_s21(&s21_params(3)).unwrap();
        assert_eq!(data.points.len(), 3);
        assert!(
            dev.transport.sent[before_retry..]
                .starts_with(b"$s11,stop\n$s21,stop\n$spec,stop\n$s21,init\n")
        );
    }

    #[test]
    fn invalid_s11_parameters_send_nothing() {
        let valid = s11_params(201);
        let invalid = [
            S11Params {
                start_hz: 0,
                ..valid.clone()
            },
            S11Params {
                stop_hz: 7_000_000_001,
                ..valid.clone()
            },
            S11Params {
                stop_hz: 1_000_999,
                ..valid.clone()
            },
            S11Params {
                points: 2,
                ..valid.clone()
            },
            S11Params {
                points: 1002,
                ..valid.clone()
            },
            S11Params {
                cal: Cal::CalOn,
                ..valid.clone()
            },
            S11Params {
                format: Format::Delay,
                ..valid.clone()
            },
            S11Params {
                rbw: Some(Rbw::R100Hz),
                ..valid
            },
        ];
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        for params in invalid {
            assert!(matches!(
                dev.sweep_s11(&params),
                Err(Error::InvalidParameter(_))
            ));
            assert!(dev.transport.sent.is_empty());
            assert_eq!(dev.active_mode, None);
        }
    }

    #[test]
    fn invalid_spec_parameters_send_nothing() {
        let valid = spec_params(201);
        let invalid = [
            SpecParams {
                start_hz: 2_000_000,
                ..valid.clone()
            },
            SpecParams {
                stop_hz: 7_000_000_001,
                ..valid.clone()
            },
            SpecParams {
                stop_hz: 1_000_999,
                ..valid.clone()
            },
            SpecParams {
                points: 2,
                ..valid.clone()
            },
            SpecParams {
                points: 1002,
                ..valid.clone()
            },
            SpecParams {
                cal: Cal::CalUser,
                ..valid.clone()
            },
            SpecParams {
                rbw: Rbw::R300Hz,
                ..valid.clone()
            },
            SpecParams {
                ref_level_dbm: 11,
                ..valid
            },
        ];
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        for params in invalid {
            assert!(matches!(
                dev.sweep_spec(&params),
                Err(Error::InvalidParameter(_))
            ));
            assert!(dev.transport.sent.is_empty());
            assert_eq!(dev.active_mode, None);
        }
    }

    #[test]
    fn invalid_request_does_not_stop_an_initialized_mode() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        dev.sweep_s11(&s11_params(3)).unwrap();
        let before = dev.transport.sent.clone();
        let invalid = SpecParams {
            cal: Cal::CalSys,
            ..spec_params(3)
        };
        assert!(matches!(
            dev.sweep_spec(&invalid),
            Err(Error::InvalidParameter(_))
        ));
        assert_eq!(dev.transport.sent, before);
        assert_eq!(dev.active_mode, Some(StreamMode::S11));
    }

    #[test]
    fn legacy_sweeps_convert_requested_samples_once() {
        for (samples, wire) in [(3, 2), (201, 200), (1001, 1000)] {
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_sweep(StreamMode::S11, samples);
            mock.queue_sweep(StreamMode::Spec, samples);
            let mut dev = Device::new(mock);
            assert_eq!(
                dev.sweep_s11(&s11_params(samples)).unwrap().points.len(),
                samples as usize
            );
            assert!(
                dev.transport
                    .sent_text()
                    .ends_with(&format!("$s11,run,caloff,loss,{wire},ss,1000000,2000000\n"))
            );
            assert_eq!(
                dev.sweep_spec(&spec_params(samples)).unwrap().points.len(),
                samples as usize
            );
            assert!(dev.transport.sent_text().ends_with(&format!(
                "$spec,run,caloff,highlo,{wire},ss,1000000,2000000\n"
            )));
        }
    }

    #[test]
    fn explicit_krj_models_do_not_subtract_samples() {
        // These are command-generation checks, not hardware verification.
        for model in [Model::Kc901K, Model::Kc901R, Model::Kc901J] {
            for samples in [2, 201, 1000] {
                let mut mock = MockTransport::with_lines(&[]);
                mock.queue_sweep(StreamMode::S11, samples);
                let mut dev = Device::with_model(mock, model);
                assert_eq!(
                    dev.sweep_s11(&s11_params(samples)).unwrap().points.len(),
                    samples as usize
                );
                assert!(dev.transport.sent_text().ends_with(&format!(
                    "$s11,run,caloff,loss,{samples},ss,1000000,2000000\n"
                )));
            }
        }
    }

    #[test]
    fn switching_modes_stops_the_previous_mode_before_initializing() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::S11, 3);
        mock.queue_sweep(StreamMode::Spec, 3);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        dev.sweep_s11(&s11_params(3)).unwrap();
        dev.sweep_spec(&spec_params(3)).unwrap();
        dev.sweep_s11(&s11_params(3)).unwrap();
        assert_eq!(dev.active_mode, Some(StreamMode::S11));
        assert_eq!(
            dev.transport.sent_text(),
            concat!(
                "$s11,stop\n$s21,stop\n$spec,stop\n$s11,init\n",
                "$s11,run,caloff,loss,2,ss,1000000,2000000\n",
                "$s11,stop\n$spec,init\n$bw,10k\n$specref,-10\n",
                "$spec,run,caloff,highlo,2,ss,1000000,2000000\n",
                "$spec,stop\n$s11,init\n",
                "$s11,run,caloff,loss,2,ss,1000000,2000000\n",
            )
        );
    }

    #[test]
    fn repeated_sweeps_reuse_the_initialized_mode() {
        for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_sweep(mode, 3);
            mock.queue_sweep(mode, 3);
            let mut dev = Device::new(mock);
            let run = |dev: &mut Device<MockTransport>| match mode {
                StreamMode::S11 => dev.sweep_s11(&s11_params(3)),
                StreamMode::S21 => dev.sweep_s21(&s21_params(3)),
                StreamMode::Spec => dev.sweep_spec(&spec_params(3)),
                _ => unreachable!(),
            };
            run(&mut dev).unwrap();
            let first_run_len = dev.transport.sent.len();
            run(&mut dev).unwrap();
            let repeated = String::from_utf8_lossy(&dev.transport.sent[first_run_len..]);
            assert!(!repeated.contains(",stop\n"));
            assert!(!repeated.contains(",init\n"));
            assert!(repeated.contains(",run,"));
            assert_eq!(dev.active_mode, Some(mode));
        }
    }

    #[test]
    fn stop_sweep_clears_mode_and_is_idempotent() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        dev.stop_sweep().unwrap();
        assert!(dev.transport.sent.is_empty());
        dev.sweep_s11(&s11_params(3)).unwrap();
        let first_run_len = dev.transport.sent.len();
        dev.stop_sweep().unwrap();
        dev.stop_sweep().unwrap();
        assert_eq!(dev.active_mode, None);
        assert_eq!(&dev.transport.sent[first_run_len..], b"$s11,stop\n");
    }

    #[test]
    fn direct_stop_failure_retires_the_session() {
        let mut dev = Device::new(MockTransport::with_lines(&[]));
        dev.active_mode = Some(StreamMode::S11);
        dev.transport.fail_send = Some(1);
        assert!(matches!(dev.stop_sweep(), Err(Error::NotConnected)));
        assert!(dev.requires_reconnect());
        assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::NotConnected)
        ));
        assert!(!dev.transport.sent_text().contains("$temp"));
        assert!(!dev.transport.sent_text().contains(",run,"));
    }

    #[test]
    fn close_stops_active_mode_before_returning_local() {
        for mode in [StreamMode::S11, StreamMode::S21, StreamMode::Spec] {
            let mut mock = MockTransport::with_lines(&["[KC901]002015123456"]);
            mock.queue_sweep(mode, 3);
            let mut dev = Device::new(mock);
            dev.handshake().unwrap();
            match mode {
                StreamMode::S11 => dev.sweep_s11(&s11_params(3)).unwrap(),
                StreamMode::S21 => dev.sweep_s21(&s21_params(3)).unwrap(),
                StreamMode::Spec => dev.sweep_spec(&spec_params(3)).unwrap(),
                _ => unreachable!(),
            };
            let before_close = dev.transport.sent.len();
            dev.close();
            assert_eq!(dev.active_mode, None);
            assert!(!dev.remote);
            let expected = match mode {
                StreamMode::S11 => b"$s11,stop\n$local\n".as_slice(),
                StreamMode::S21 => b"$s21,stop\n$local\n".as_slice(),
                StreamMode::Spec => b"$spec,stop\n$local\n".as_slice(),
                _ => unreachable!(),
            };
            assert_eq!(&dev.transport.sent[before_close..], expected);
            let after_close = dev.transport.sent.len();
            dev.close();
            assert_eq!(dev.transport.sent.len(), after_close);
        }
    }

    #[test]
    fn unknown_bandwidth_uses_the_slowest_supported_timeout() {
        for model in [Model::Kc901V, Model::Kc901K] {
            let dev = Device::with_model(MockTransport::with_lines(&[]), model);
            let slowest = model
                .capabilities()
                .rbw_list
                .iter()
                .map(|&rbw| model::sweep_timeout(Some(rbw), 1001))
                .max()
                .unwrap();
            assert_eq!(dev.sweep_timeout(1001), slowest);
        }
    }

    #[test]
    fn s11_without_bandwidth_inherits_the_previous_spec_timeout() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_sweep(StreamMode::Spec, 3);
        mock.queue_sweep(StreamMode::S11, 1001);
        let mut dev = Device::new(mock);
        let params = SpecParams {
            rbw: Rbw::R1k,
            ..spec_params(3)
        };
        dev.sweep_spec(&params).unwrap();
        let before_s11 = dev.transport.sent.len();
        dev.sweep_s11(&s11_params(1001)).unwrap();
        assert_eq!(dev.last_rbw, Some(Rbw::R1k));
        assert_eq!(
            dev.sweep_timeout(1001),
            model::sweep_timeout(Some(Rbw::R1k), 1001)
        );
        let sent = String::from_utf8_lossy(&dev.transport.sent[before_s11..]);
        assert!(!sent.contains("$bw,"));
    }

    #[test]
    fn failed_initialization_is_retried_on_the_next_sweep() {
        let mut mock = MockTransport::with_lines(&[
            "$start,err_uninit",
            "$error:Please initialize the mode first!",
            "$end",
        ]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.sweep_s11(&s11_params(3)), Err(Error::Device(ref name)) if name == "err_uninit")
        );
        assert_eq!(dev.active_mode, None);
        assert_eq!(dev.last_rbw, None);
        assert!(!dev.requires_reconnect());
        let before_retry = dev.transport.sent.len();
        dev.sweep_s11(&s11_params(3)).unwrap();
        assert_eq!(dev.active_mode, Some(StreamMode::S11));
        assert!(
            dev.transport.sent[before_retry..]
                .starts_with(b"$s11,stop\n$s21,stop\n$spec,stop\n$s11,init\n")
        );
    }

    #[test]
    fn setup_and_run_send_failures_require_a_fresh_connection() {
        // A send failure can leave an unknown amount of a command on the wire.
        for failed_send in 1..=6 {
            let mut mock = MockTransport::with_lines(&[]);
            mock.fail_send = Some(failed_send);
            mock.queue_sweep(StreamMode::S11, 3);
            let mut dev = Device::new(mock);
            let params = S11Params {
                rbw: Some(Rbw::R10k),
                ..s11_params(3)
            };
            assert!(matches!(dev.sweep_s11(&params), Err(Error::NotConnected)));
            assert_eq!(dev.active_mode, None);
            assert_eq!(dev.last_rbw, None);
            assert!(dev.requires_reconnect());
            let before_retry = dev.transport.sent.len();
            assert!(matches!(dev.sweep_s11(&params), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent.len(), before_retry);
        }
    }

    #[test]
    fn rejected_frame_cannot_leak_into_another_request() {
        let mut mock = MockTransport::with_lines(&[
            "$start,s11,ma",
            "$1000000,1,2",
            "$1500000,3,4",
            "$2000000,5,6",
            "$end",
        ]);
        mock.queue_sweep(StreamMode::S11, 3);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::Protocol(_))
        ));
        assert!(dev.requires_reconnect());
        let sent = dev.transport.sent.clone();
        let remaining = dev.transport.incoming.clone();
        assert!(!remaining.is_empty());
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::NotConnected)
        ));
        assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
        assert_eq!(dev.transport.sent, sent);
        assert_eq!(dev.transport.incoming, remaining);
    }

    #[test]
    fn timed_out_query_cannot_consume_a_late_reply_on_retry() {
        let mut mock = MockTransport::with_lines(&["$start,temp", "$47.3", "$end"]);
        mock.read_delay = Duration::from_millis(20);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.query_packet(commands::TEMP, "temp", Duration::from_millis(5)),
            Err(Error::Timeout)
        ));
        assert!(dev.requires_reconnect());
        let remaining = dev.transport.incoming.clone();
        let sent = dev.transport.sent.clone();
        assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
        assert!(matches!(dev.handshake(), Err(Error::NotConnected)));
        assert_eq!(dev.transport.incoming, remaining);
        assert_eq!(dev.transport.sent, sent);
    }

    #[test]
    fn malformed_query_fields_require_a_fresh_connection() {
        for name in ["device", "temp", "voltage"] {
            let header = format!("$start,{name}");
            let mock = MockTransport::with_lines(&[&header, "$invalid", "$end"]);
            let mut dev = Device::new(mock);
            let result = match name {
                "device" => dev.device_info().map(|_| ()),
                "temp" => dev.temperature().map(|_| ()),
                "voltage" => dev.voltage().map(|_| ()),
                _ => unreachable!(),
            };
            assert!(matches!(result, Err(Error::Protocol(_))), "{name}");
            assert!(dev.requires_reconnect(), "{name}");
            let sent = dev.transport.sent.clone();
            assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
            assert_eq!(dev.transport.sent, sent);
        }
    }

    #[test]
    fn malformed_telemetry_retires_the_session_before_a_later_valid_reply() {
        for name in ["temp", "voltage"] {
            let header = format!("$start,{name}");
            let valid = if name == "temp" { "$-12.5" } else { "$0,8.03" };
            let invalid_bodies: &[&[&str]] = if name == "temp" {
                &[
                    &[],
                    &["$47,48"],
                    &["$47", "$48"],
                    &["$NaN"],
                    &["$-Inf"],
                    &["$1e999"],
                ]
            } else {
                &[
                    &[],
                    &["$12"],
                    &["$12,8,7"],
                    &["$12,8", "$12,8"],
                    &["$NaN,8"],
                    &["$12,+Inf"],
                    &["$-1e999,8"],
                ]
            };
            for body in invalid_bodies {
                let mut lines = vec![header.as_str()];
                lines.extend_from_slice(body);
                lines.extend(["$end", header.as_str(), valid, "$end"]);
                let mut dev = Device::new(MockTransport::with_lines(&lines));
                let result = if name == "temp" {
                    dev.temperature().map(|_| ())
                } else {
                    dev.voltage().map(|_| ())
                };
                assert!(matches!(result, Err(Error::Protocol(_))), "{name} {body:?}");
                assert!(dev.requires_reconnect());
                let remaining = dev.transport.incoming.clone();
                let sent = dev.transport.sent.clone();
                assert_eq!(remaining.len(), 3);
                assert!(matches!(dev.temperature(), Err(Error::NotConnected)));
                assert!(matches!(dev.voltage(), Err(Error::NotConnected)));
                assert_eq!(dev.transport.incoming, remaining);
                assert_eq!(dev.transport.sent, sent);
            }
        }
    }

    #[test]
    fn finite_signed_telemetry_keeps_the_session_usable() {
        let mock = MockTransport::with_lines(&[
            "$start,temp",
            "$-12.5",
            "$end",
            "$start,voltage",
            "$-0,-1.25",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        assert_eq!(dev.temperature().unwrap(), -12.5);
        let voltage = dev.voltage().unwrap();
        assert_eq!(voltage.external.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(voltage.battery, -1.25);
        assert!(!dev.requires_reconnect());
        assert_eq!(dev.transport.sent_text(), "$temp\n$voltage\n");
    }

    #[test]
    fn cleanup_failure_preserves_the_original_sweep_error() {
        let mut mock = MockTransport::with_lines(&[
            "$start,err_uninit",
            "$error:Please initialize the mode first!",
            "$end",
        ]);
        // Fail the cleanup stop after the three initial stops, init and run.
        mock.fail_send = Some(6);
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.sweep_s11(&s11_params(3)), Err(Error::Device(ref name)) if name == "err_uninit")
        );
        assert_eq!(dev.transport.send_calls, 6);
        assert_eq!(dev.active_mode, None);
        assert_eq!(dev.last_rbw, None);
        assert!(dev.requires_reconnect());
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::NotConnected)
        ));
        assert_eq!(dev.transport.send_calls, 6);
    }

    #[test]
    fn sweep_aborts_on_err_uninit() {
        let mock = MockTransport::with_lines(&[
            "$start,err_uninit",
            "$error:Please initialize the mode first!",
            "$end",
        ]);
        let mut dev = Device::new(mock);
        let params = SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 10,
            start_hz: 75_000_000,
            stop_hz: 125_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        };
        assert!(matches!(dev.sweep_spec(&params), Err(Error::Device(ref n)) if n == "err_uninit"));
    }

    fn source_params(kind: SourceKind) -> SourceParams {
        SourceParams {
            kind,
            port: SourcePort::Port1,
            frequency_hz: 1_000_000,
            amplitude: SourceAmplitude::Dbm(-10),
            modulation: Modulation::Off,
        }
    }

    fn active_source(mock: MockTransport, kind: SourceKind) -> Device<MockTransport> {
        let mut dev = Device::new(mock);
        dev.source_kind = Some(kind);
        dev.source.state = SourceOutputState::Requested(kind);
        dev.release_on_close = true;
        dev
    }

    #[test]
    fn source_start_is_fenced_and_warnings_are_not_output_acknowledgements() {
        for kind in [SourceKind::Rf, SourceKind::Af] {
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.queue_identity();
            mock.incoming
                .extend(["$start,warn_gtr", "$clamped", "$end"].map(str::to_owned));
            let mut dev = Device::new(mock);
            let report = dev.start_source(&source_params(kind)).unwrap();
            assert_eq!(report.state, SourceOutputState::Requested(kind));
            assert_eq!(report.warning, Some(SourceWarning::AboveMaximum));
            assert_eq!(dev.source_kind, Some(kind));
            assert!(!dev.requires_reconnect());
            assert_eq!(
                dev.transport.sent_text(),
                format!(
                    "$s11,stop\n$s21,stop\n$spec,stop\n$rfsource,stop\n$afsource,stop\n$device\n{}$device\n{}",
                    kind.init_command(),
                    source_params(kind).command(),
                )
            );
            assert!(
                dev.transport
                    .read_timeouts
                    .iter()
                    .all(|time| *time <= POLL_INTERVAL)
            );

            dev.transport
                .incoming
                .extend(["$start,warn_lt", "$clamped", "$end"].map(str::to_owned));
            dev.transport.queue_identity();
            let report = dev.stop_source().unwrap();
            assert_eq!(report.state, SourceOutputState::StopSent);
            assert_eq!(report.warning, Some(SourceWarning::BelowMinimum));
            assert_eq!(dev.source_kind, None);
            assert!(!dev.requires_reconnect());
            assert!(
                dev.transport
                    .sent_text()
                    .ends_with(&format!("{}$device\n", kind.stop_command()))
            );
        }
    }

    #[test]
    fn source_replacement_stops_the_previous_kind_and_discards_its_warning() {
        let mut mock = MockTransport::with_lines(&["$start,warn_gtr", "$clamped", "$end"]);
        mock.queue_identity();
        mock.queue_identity();
        let mut dev = active_source(mock, SourceKind::Rf);
        let report = dev.start_source(&source_params(SourceKind::Af)).unwrap();
        assert_eq!(report.state, SourceOutputState::Requested(SourceKind::Af));
        assert_eq!(report.warning, None);
        assert_eq!(
            dev.transport.sent_text(),
            "$s11,stop\n$s21,stop\n$spec,stop\n$rfsource,stop\n$device\n$afsource,init\n$device\n$afsource,run,off,port1,1000000,-10,0,0\n"
        );
    }

    #[test]
    fn source_preflight_rejections_do_not_send_or_change_the_report() {
        let mut dev = active_source(MockTransport::with_lines(&[]), SourceKind::Rf);
        let report = dev.source_report();
        let mut params = source_params(SourceKind::Rf);
        params.frequency_hz = 7_000_000_001;
        assert!(matches!(
            dev.start_source(&params),
            Err(Error::InvalidParameter(_))
        ));
        let cancel = CancellationToken::default();
        cancel.cancel();
        params.frequency_hz = 1_000_000;
        assert!(matches!(
            dev.start_source_controlled(&params, &cancel),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            dev.stop_source_controlled(&cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(dev.source_report(), report);
        assert_eq!(dev.transport.send_calls, 0);
        assert!(dev.transport.read_timeouts.is_empty());

        let mut dev = Device::new(MockTransport::with_lines(&[]));
        dev.active_mode = Some(StreamMode::S11);
        assert!(matches!(
            dev.start_source(&params),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(dev.stop_source(), Err(Error::InvalidParameter(_))));
        assert_eq!(dev.source_report(), SourceReport::default());
        assert_eq!(dev.transport.send_calls, 0);
    }

    #[test]
    fn acquisition_cannot_interrupt_or_reset_a_source_during_preflight() {
        let mut dev = active_source(MockTransport::with_lines(&[]), SourceKind::Rf);
        dev.packets.feed_line("$start,warn_gtr").unwrap();
        let report = dev.source_report();
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            dev.sweep_s21(&s21_params(3)),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            dev.sweep_spec(&spec_params(3)),
            Err(Error::InvalidParameter(_))
        ));
        assert!(matches!(
            dev.measure_point(&point_params(StreamMode::S11)),
            Err(Error::InvalidParameter(_))
        ));
        assert_eq!(dev.transport.send_calls, 0);
        assert_eq!(dev.source_report(), report);
        let packet = dev.packets.feed_line("$end").unwrap().unwrap();
        assert_eq!(packet.name, "warn_gtr");
    }

    #[test]
    fn source_cancel_after_init_or_run_cleans_up_without_resuming_output() {
        for cancel_send in [7, 9] {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            if cancel_send == 9 {
                mock.queue_identity();
            }
            mock.queue_identity();
            mock.cancel_on_send = Some((cancel_send, cancel.clone()));
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.start_source_controlled(&source_params(SourceKind::Rf), &cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
            assert_eq!(dev.source_kind, None);
            assert!(!dev.requires_reconnect());
            assert!(
                dev.transport
                    .sent_text()
                    .ends_with("$rfsource,stop\n$device\n")
            );
            assert!(!dev.transport.sent_text().contains("$local"));
            assert_eq!(
                dev.transport.sent_text().matches(",run,").count(),
                usize::from(cancel_send == 9)
            );
        }
    }

    #[test]
    fn cancelling_initial_source_normalization_does_not_initialize_output() {
        for cancel_send in 1..=5 {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.cancel_on_send = Some((cancel_send, cancel.clone()));
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.start_source_controlled(&source_params(SourceKind::Rf), &cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
            assert!(!dev.requires_reconnect());
            assert!(
                dev.transport
                    .sent_text()
                    .ends_with("$rfsource,stop\n$afsource,stop\n$device\n")
            );
            assert!(!dev.transport.sent_text().contains(",init"));
            assert!(!dev.transport.sent_text().contains(",run,"));
        }
    }

    #[test]
    fn uncertain_source_requires_explicit_stop_before_acquiring_again() {
        let cancel = CancellationToken::default();
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_identity();
        mock.queue_identity();
        mock.queue_identity();
        mock.cancel_on_send = Some((9, cancel.clone()));
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.start_source_controlled(&source_params(SourceKind::Rf), &cancel),
            Err(Error::Cancelled)
        ));
        assert!(!dev.requires_reconnect());
        assert_eq!(dev.source_kind, None);
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        let sends = dev.transport.send_calls;
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::InvalidParameter(_))
        ));
        assert_eq!(dev.transport.send_calls, sends);
        dev.transport.queue_identity();
        assert_eq!(
            dev.stop_source().unwrap().state,
            SourceOutputState::StopSent
        );
        dev.transport.queue_sweep(StreamMode::S11, 3);
        assert_eq!(dev.sweep_s11(&s11_params(3)).unwrap().points.len(), 3);
        assert_eq!(dev.source_report().state, SourceOutputState::StopSent);
    }

    #[test]
    fn cancelled_source_stop_keeps_unknown_and_never_reuses_its_identity() {
        for cancel_send in [1, 2] {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.cancel_on_send = Some((cancel_send, cancel.clone()));
            let mut dev = active_source(mock, SourceKind::Rf);
            assert!(matches!(
                dev.stop_source_controlled(&cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
            assert_eq!(dev.transport.sent_text().matches("$device\n").count(), 1);
            assert_eq!(dev.requires_reconnect(), cancel_send == 2);
            assert_eq!(dev.transport.incoming.is_empty(), cancel_send == 1);
        }
    }

    #[test]
    fn missing_source_stop_fence_retires_the_connection() {
        let mut dev = active_source(MockTransport::with_lines(&[]), SourceKind::Rf);
        assert!(matches!(dev.stop_source(), Err(Error::Timeout)));
        assert!(dev.requires_reconnect());
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        assert_eq!(
            dev.transport.sent_text(),
            "$rfsource,stop\n$device\n$rfsource,stop\n$local\n"
        );
        assert!(
            dev.transport
                .read_timeouts
                .iter()
                .all(|time| *time <= POLL_INTERVAL)
        );
    }

    #[test]
    fn cancelled_source_identity_fences_never_reuse_a_late_identity() {
        for cancel_send in [6, 8] {
            let cancel = CancellationToken::default();
            let mut mock = MockTransport::with_lines(&[]);
            mock.queue_identity();
            mock.queue_identity();
            mock.cancel_on_send = Some((cancel_send, cancel.clone()));
            let mut dev = Device::new(mock);
            assert!(matches!(
                dev.start_source_controlled(&source_params(SourceKind::Rf), &cancel),
                Err(Error::Cancelled)
            ));
            assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
            assert!(dev.requires_reconnect());
            let sent = dev.transport.sent_text();
            assert_eq!(
                sent.matches("$device\n").count(),
                if cancel_send == 6 { 1 } else { 2 }
            );
            assert!(sent.ends_with("$local\n"));
            assert!(!sent.contains(",run,"));
            assert!(!dev.transport.incoming.is_empty());
            let sends = dev.transport.send_calls;
            assert!(matches!(dev.device_info(), Err(Error::NotConnected)));
            assert!(matches!(
                dev.start_source(&source_params(SourceKind::Af)),
                Err(Error::NotConnected)
            ));
            assert_eq!(dev.transport.send_calls, sends);
        }
    }

    #[test]
    fn source_stop_rejects_malformed_or_error_fences_and_keeps_unknown() {
        for lines in [
            vec!["$start,device", "$invalid", "$end"],
            vec!["$start,err_RfStop", "$stop failed", "$end"],
        ] {
            let mut mock = MockTransport::with_lines(&lines);
            // This later packet must not become a second cleanup query's reply.
            mock.queue_identity();
            let mut dev = active_source(mock, SourceKind::Rf);
            assert!(dev.stop_source().is_err());
            assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
            assert!(dev.requires_reconnect());
            assert_eq!(
                dev.transport.sent_text(),
                "$rfsource,stop\n$device\n$rfsource,stop\n$local\n"
            );
            assert!(!dev.transport.incoming.is_empty());
        }
    }

    #[test]
    fn source_run_write_failure_closes_without_claiming_stopped() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_identity();
        mock.queue_identity();
        mock.fail_send = Some(9);
        let mut dev = Device::new(mock);
        assert!(matches!(
            dev.start_source(&source_params(SourceKind::Rf)),
            Err(Error::NotConnected)
        ));
        assert!(dev.requires_reconnect());
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        assert!(
            dev.transport
                .sent_text()
                .ends_with("$rfsource,stop\n$local\n")
        );
        assert_eq!(dev.transport.sent_text().matches("$device\n").count(), 2);
    }

    #[test]
    fn source_error_preserves_the_original_error_when_cleanup_also_fails() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.queue_identity();
        mock.queue_identity();
        mock.incoming.extend(
            [
                "$start,err_par4",
                "$invalid amplitude",
                "$end",
                "$start,err_RfStop",
                "$stop failed",
                "$end",
            ]
            .map(str::to_owned),
        );
        let mut dev = Device::new(mock);
        assert!(
            matches!(dev.start_source(&source_params(SourceKind::Rf)), Err(Error::Device(name)) if name == "err_par4")
        );
        assert!(dev.requires_reconnect());
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        assert!(
            dev.transport
                .sent_text()
                .ends_with("$rfsource,stop\n$device\n$rfsource,stop\n$local\n")
        );
    }

    #[test]
    fn source_polls_preserve_partial_warnings_and_late_errors() {
        let mut dev = active_source(
            MockTransport::with_lines(&["$start,warn_gtr"]),
            SourceKind::Rf,
        );
        let cancel = CancellationToken::default();
        assert_eq!(dev.poll_source_controlled(&cancel).unwrap().warning, None);
        dev.transport
            .incoming
            .extend(["$clamped", "$end"].map(str::to_owned));
        assert_eq!(
            dev.poll_source_controlled(&cancel).unwrap().warning,
            Some(SourceWarning::AboveMaximum)
        );
        dev.transport
            .incoming
            .extend(["$start,err_par4", "$late rejection", "$end"].map(str::to_owned));
        assert!(
            matches!(dev.poll_source_controlled(&cancel), Err(Error::Device(name)) if name == "err_par4")
        );
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        assert_eq!(
            dev.source_report().warning,
            Some(SourceWarning::AboveMaximum)
        );
        assert_eq!(dev.source_kind, Some(SourceKind::Rf));
        assert_eq!(dev.transport.send_calls, 0);
        assert!(
            dev.transport
                .read_timeouts
                .iter()
                .all(|time| *time <= POLL_INTERVAL)
        );
        assert!(matches!(
            dev.sweep_s11(&s11_params(3)),
            Err(Error::InvalidParameter(_))
        ));
        assert_eq!(dev.transport.send_calls, 0);
    }

    #[test]
    fn inactive_source_poll_performs_no_io() {
        let mut dev = Device::new(MockTransport::with_lines(&["$start,warn_gtr", "$end"]));
        assert_eq!(
            dev.poll_source_controlled(&CancellationToken::default())
                .unwrap(),
            SourceReport::default()
        );
        assert!(dev.transport.read_timeouts.is_empty());
        assert_eq!(dev.transport.send_calls, 0);
    }

    #[test]
    fn queries_preserve_source_warnings_and_retire_uncertain_replies() {
        let mut dev = active_source(
            MockTransport::with_lines(&[
                "$start,warn_lt",
                "$clamped",
                "$end",
                "$start,temp",
                "$42",
                "$end",
            ]),
            SourceKind::Rf,
        );
        assert_eq!(dev.temperature().unwrap(), 42.0);
        assert_eq!(
            dev.source_report().warning,
            Some(SourceWarning::BelowMinimum)
        );
        assert_eq!(
            dev.source_report().state,
            SourceOutputState::Requested(SourceKind::Rf)
        );
        dev.transport
            .incoming
            .extend(["$start,err_par1", "$query rejected", "$end"].map(str::to_owned));
        dev.transport.queue_identity();
        assert!(matches!(dev.device_info(), Err(Error::Device(name)) if name == "err_par1"));
        assert!(dev.requires_reconnect());
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        let sends = dev.transport.send_calls;
        assert!(matches!(dev.stop_source(), Err(Error::NotConnected)));
        assert_eq!(dev.transport.send_calls, sends);
    }

    #[test]
    fn closing_a_source_stops_before_local_with_one_total_budget() {
        let mut mock = MockTransport::with_lines(&[]);
        mock.send_delay = Duration::from_millis(10);
        let mut dev = active_source(mock, SourceKind::Af);
        dev.close();
        assert_eq!(dev.transport.sent_text(), "$afsource,stop\n$local\n");
        assert_eq!(dev.source_report().state, SourceOutputState::Unknown);
        assert!(dev.requires_reconnect());
        assert!(dev.transport.write_timeouts[1] < dev.transport.write_timeouts[0]);
        let sends = dev.transport.send_calls;
        dev.close();
        assert_eq!(dev.transport.send_calls, sends);
    }
}
