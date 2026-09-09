// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Signal-source command intent and bounded warning observation.

use std::time::{Duration, Instant};

use super::{CLEANUP_TIMEOUT, COMMAND_GAP, Device};
use crate::commands;
use crate::control::{CancellationToken, POLL_INTERVAL};
use crate::data::DeviceInfo;
use crate::error::{Error, Result};
use crate::protocol::Packet;
use crate::source::{SourceKind, SourceOutputState, SourceParams, SourceReport, SourceWarning};
use crate::transport::{GENERIC_TIMEOUT, Transport, remaining_timeout};

/// A host observation window, not a source acknowledgement deadline (section 5.2).
const SOURCE_WARNING_WINDOW: Duration = Duration::from_secs(1);

impl<T: Transport> Device<T> {
    /// Last source command intent and any observed amplitude warning. Neither
    /// Requested nor StopSent is a measurement of the physical output state.
    pub fn source_report(&self) -> SourceReport {
        self.source
    }

    pub fn start_source(&mut self, params: &SourceParams) -> Result<SourceReport> {
        self.start_source_controlled(params, &CancellationToken::default())
    }

    /// Apply an explicitly requested source configuration. The receiver mode
    /// must already be stopped. Existing source output is stopped and fenced
    /// before reinitialization, including when changing the source port.
    pub fn start_source_controlled(
        &mut self,
        params: &SourceParams,
        cancel: &CancellationToken,
    ) -> Result<SourceReport> {
        cancel.check()?;
        params.validate(&self.caps)?;
        self.check_source_access()?;
        self.source.state = SourceOutputState::Unknown;
        self.release_on_close = true;
        let result = (|| {
            self.stop_source_fenced(CLEANUP_TIMEOUT, cancel, true)?;
            cancel.check()?;
            self.source.state = SourceOutputState::Unknown;
            self.source.warning = None;
            self.source_kind = Some(params.kind);
            let started = Instant::now();
            self.transport
                .send_with_timeout(params.kind.init_command().as_bytes(), GENERIC_TIMEOUT)?;
            cancel.pause(COMMAND_GAP)?;
            self.source_identity_fence(started, GENERIC_TIMEOUT, cancel)?;
            self.send_controlled(params.command().as_bytes(), cancel)?;
            self.observe_source(SOURCE_WARNING_WINDOW, cancel)?;
            cancel.check()?;
            self.source.state = SourceOutputState::Requested(params.kind);
            Ok(self.source)
        })();
        self.finish_source_action(result)
    }

    pub fn stop_source(&mut self) -> Result<SourceReport> {
        self.stop_source_controlled(&CancellationToken::default())
    }

    /// Send source stop commands and require a fresh identity boundary.
    /// StopSent confirms this protocol sequence, not physical output shutdown.
    pub fn stop_source_controlled(&mut self, cancel: &CancellationToken) -> Result<SourceReport> {
        cancel.check()?;
        self.check_source_access()?;
        self.source.state = SourceOutputState::Unknown;
        self.release_on_close = true;
        let result = self
            .stop_source_fenced(CLEANUP_TIMEOUT, cancel, false)
            .and_then(|()| {
                cancel.check()?;
                self.source.state = SourceOutputState::StopSent;
                Ok(self.source)
            });
        self.finish_source_action(result)
    }

    /// Observe at most one 50 ms host window. With no active source this does
    /// no I/O. Late device errors remain errors and change the report to Unknown.
    pub fn poll_source_controlled(&mut self, cancel: &CancellationToken) -> Result<SourceReport> {
        cancel.check()?;
        if self.source_kind.is_none() {
            return Ok(self.source);
        }
        if self.requires_reconnect() {
            self.source.state = SourceOutputState::Unknown;
            return Err(Error::NotConnected);
        }
        let result = self
            .observe_source(POLL_INTERVAL, cancel)
            .map(|()| self.source);
        self.record_result(result)
    }

    fn check_source_access(&self) -> Result<()> {
        if self.requires_reconnect() {
            return Err(Error::NotConnected);
        }
        if self.active_mode.is_some() {
            return Err(Error::InvalidParameter(
                "stop the acquisition mode before controlling a signal source".into(),
            ));
        }
        Ok(())
    }

    fn source_identity_fence(
        &mut self,
        started: Instant,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        cancel.check()?;
        let result = (|| {
            self.transport.send_with_timeout(
                commands::DEVICE.as_bytes(),
                remaining_timeout(started, timeout)?,
            )?;
            let packet = self.expect_controlled("device", started, timeout, cancel)?;
            DeviceInfo::from_packet(&packet)?;
            Ok(())
        })();
        if result.is_err() {
            // A late identity from this query cannot be distinguished from a
            // reply to another query. Do not attempt a second receive fence.
            self.session_failed = true;
        }
        result
    }

    fn stop_source_fenced(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
        stop_receivers: bool,
    ) -> Result<()> {
        let started = Instant::now();
        if stop_receivers {
            // A fresh remote session can retain a front-panel receiver mode.
            // Normalize the supported receivers before initializing a source.
            for command in [commands::S11_STOP, commands::S21_STOP, commands::SPEC_STOP] {
                cancel.check()?;
                self.transport
                    .send_with_timeout(command.as_bytes(), remaining_timeout(started, timeout)?)?;
            }
        }
        // An ordered identity reply is the receive boundary. A socket write or
        // an unrelated measurement end is not a source acknowledgement.
        for &kind in self.source_stop_kinds() {
            cancel.check()?;
            self.transport.send_with_timeout(
                kind.stop_command().as_bytes(),
                remaining_timeout(started, timeout)?,
            )?;
        }
        self.source_identity_fence(started, timeout, cancel)?;
        self.source_kind = None;
        self.reset_measurement();
        Ok(())
    }

    pub(super) fn source_stop_kinds(&self) -> &'static [SourceKind] {
        match self.source_kind {
            Some(SourceKind::Rf) => &[SourceKind::Rf],
            Some(SourceKind::Af) => &[SourceKind::Af],
            None => &[SourceKind::Rf, SourceKind::Af],
        }
    }

    fn finish_source_action(&mut self, result: Result<SourceReport>) -> Result<SourceReport> {
        let result = self.record_result(result);
        if result.is_err() {
            self.source.state = SourceOutputState::Unknown;
            // Cancellation cannot abandon a possibly enabled output. Cleanup
            // has its own bounded budget and never resumes a source command.
            if self.requires_reconnect()
                || self
                    .stop_source_fenced(CLEANUP_TIMEOUT, &CancellationToken::default(), false)
                    .is_err()
            {
                self.close();
            }
            self.source.state = SourceOutputState::Unknown;
        }
        result
    }

    fn observe_source(&mut self, timeout: Duration, cancel: &CancellationToken) -> Result<()> {
        let started = Instant::now();
        loop {
            cancel.check()?;
            let Some(remaining) = timeout
                .checked_sub(started.elapsed())
                .filter(|time| !time.is_zero())
            else {
                return Ok(());
            };
            match self.transport.recv_line(remaining.min(POLL_INTERVAL)) {
                Ok(line) => {
                    if let Some(packet) = self.packets.feed_line(&line)? {
                        self.note_source_packet(&packet);
                        if packet.is_error() {
                            return Err(Error::Device(packet.name));
                        }
                    }
                    cancel.check()?;
                }
                Err(Error::Timeout) => {}
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn note_source_packet(&mut self, packet: &Packet) {
        let warning = match packet.name.as_str() {
            "warn_gtr" => SourceWarning::AboveMaximum,
            "warn_lt" => SourceWarning::BelowMinimum,
            _ => return,
        };
        self.source.warning = Some(warning);
    }
}
