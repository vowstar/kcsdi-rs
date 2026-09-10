// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Legacy KC901V calibration requests and protocol progress (section 6).
//!
//! User frequency and count checks are reference-based host acceptance bounds.
//! The manual does not specify separate numeric user-calibration limits.
//! System calibration has no caller-supplied frequency or count fields.

use std::time::Duration;

use crate::commands;
use crate::model::{Capabilities, FreqRange, Model, Rbw, sweep_timeout};
use crate::protocol::StreamMode;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationKind {
    S11System,
    S21System,
    S11User,
    S21User,
}

impl CalibrationKind {
    pub fn is_user(self) -> bool {
        matches!(self, Self::S11User | Self::S21User)
    }

    pub fn mode(self) -> StreamMode {
        match self {
            Self::S11System | Self::S11User => StreamMode::S11,
            Self::S21System | Self::S21User => StreamMode::S21,
        }
    }

    pub fn init_command(self) -> &'static str {
        match self {
            Self::S11System | Self::S11User => commands::S11_INIT,
            Self::S21System | Self::S21User => commands::S21_INIT,
        }
    }

    pub fn stop_command(self) -> &'static str {
        match self {
            Self::S11System | Self::S11User => commands::S11_STOP,
            Self::S21System | Self::S21User => commands::S21_STOP,
        }
    }

    /// Command name without framing or parameters.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::S11System => "cal_s11",
            Self::S21System => "cal_s21",
            Self::S11User => "cal_user_s11",
            Self::S21User => "cal_user_s21",
        }
    }

    /// Host acceptance envelope for user calibration, not firmware limits.
    /// System calibration does not use the ordinary measurement range.
    pub fn user_range(self, caps: &Capabilities) -> Option<FreqRange> {
        match self {
            Self::S11User => Some(caps.s11.range),
            Self::S21User => Some(caps.s21.range),
            Self::S11System | Self::S21System => None,
        }
    }
}

/// A finite, uniform user-calibration definition. Frequency lists cannot be
/// represented by this command. `points` includes both requested endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserCalibrationParams {
    pub center_hz: u64,
    pub span_hz: u64,
    pub points: u32,
    pub rbw: Rbw,
}

impl UserCalibrationParams {
    /// Preserve integer endpoints using floor(center) for an odd span.
    /// Model-specific checks are performed by `CalibrationParams::validate`.
    pub fn from_range(start_hz: u64, stop_hz: u64, points: u32, rbw: Rbw) -> Result<Self> {
        let span_hz = stop_hz
            .checked_sub(start_hz)
            .filter(|span| *span > 0)
            .ok_or_else(|| invalid("user calibration needs an ascending finite range"))?;
        Ok(Self {
            center_hz: start_hz + span_hz / 2,
            span_hz,
            points,
            rbw,
        })
    }

    /// Integer equivalent of ceil(center - span/2), ceil(center + span/2).
    pub fn endpoints(self) -> Result<(u64, u64)> {
        let start_hz = self
            .center_hz
            .checked_sub(self.span_hz / 2)
            .ok_or_else(|| invalid("user calibration range starts below zero"))?;
        let stop_hz = start_hz
            .checked_add(self.span_hz)
            .ok_or_else(|| invalid("user calibration range exceeds integer frequency limits"))?;
        Ok((start_hz, stop_hz))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationParams {
    S11System,
    S21System,
    S11User(UserCalibrationParams),
    S21User(UserCalibrationParams),
}

impl CalibrationParams {
    pub fn kind(self) -> CalibrationKind {
        match self {
            Self::S11System => CalibrationKind::S11System,
            Self::S21System => CalibrationKind::S21System,
            Self::S11User(_) => CalibrationKind::S11User,
            Self::S21User(_) => CalibrationKind::S21User,
        }
    }

    pub fn user(self) -> Option<UserCalibrationParams> {
        match self {
            Self::S11User(params) | Self::S21User(params) => Some(params),
            Self::S11System | Self::S21System => None,
        }
    }

    /// Validate before any mode initialization or calibration command.
    pub fn validate(self, caps: &Capabilities) -> Result<()> {
        if caps.model != Model::Kc901V {
            return Err(invalid(
                "calibration is currently supported only for KC901V",
            ));
        }
        let mode = match self.kind().mode() {
            StreamMode::S11 => caps.s11,
            _ => caps.s21,
        };
        if !mode.enabled {
            return Err(invalid("the requested calibration mode is unavailable"));
        }
        if let Some(user) = self.user() {
            let (start_hz, stop_hz) = user.endpoints()?;
            if !mode.range.contains_sweep(start_hz, stop_hz) {
                return Err(invalid(
                    "user calibration exceeds the host frequency envelope",
                ));
            }
            if !(caps.points_min..=caps.points_max).contains(&user.points)
                || user.points <= caps.excess_points
            {
                return Err(invalid(
                    "user calibration point count is outside the host bounds",
                ));
            }
        }
        if !caps.rbw_list.contains(&self.rbw(caps)) {
            return Err(invalid("calibration RBW is unsupported by this model"));
        }
        Ok(())
    }

    pub fn rbw(self, caps: &Capabilities) -> Rbw {
        self.user().map_or(caps.system_cal_rbw, |user| user.rbw)
    }

    /// Per-measurement host deadline, not a promised calibration duration.
    /// The system count is used directly and is not sent to the instrument.
    /// Call `validate` before using this budget to start calibration.
    pub fn measurement_timeout(self, caps: &Capabilities) -> Duration {
        let points = self.user().map_or(caps.system_cal_count, |user| {
            user.points.saturating_sub(caps.excess_points)
        });
        sweep_timeout(Some(self.rbw(caps)), points)
    }

    /// Build a validated legacy command. Convert displayed points to wire
    /// count here exactly once. System commands have no numeric arguments.
    pub fn command(self, caps: &Capabilities) -> Result<String> {
        self.validate(caps)?;
        let name = self.kind().wire_name();
        Ok(match self.user() {
            Some(user) => format!(
                "${name},{},{},{}\n",
                user.center_hz,
                user.span_hz,
                user.points - caps.excess_points
            ),
            None => format!("${name}\n"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationPrompt {
    Short,
    Open,
    Load,
    Through,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CalibrationPhase {
    #[default]
    NotStarted,
    AwaitingConfirmation,
    WarmingUp,
    Prompt(CalibrationPrompt),
    Measuring,
    Processing,
    Saving,
    Completed,
    Cancelled,
    Unknown,
}

impl CalibrationPhase {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::AwaitingConfirmation
                | Self::WarmingUp
                | Self::Prompt(_)
                | Self::Measuring
                | Self::Processing
                | Self::Saving
        )
    }
}

/// Device-reported protocol progress. Completed does not assess the quality
/// of the connected calibration standards or the resulting measurements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalibrationReport {
    pub kind: Option<CalibrationKind>,
    pub phase: CalibrationPhase,
}

impl CalibrationReport {
    pub fn is_active(self) -> bool {
        self.phase.is_active()
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidParameter(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(start_hz: u64, stop_hz: u64, points: u32) -> UserCalibrationParams {
        UserCalibrationParams::from_range(start_hz, stop_hz, points, Rbw::R10k).unwrap()
    }

    #[test]
    fn user_commands_convert_display_count_once() {
        let caps = Model::Kc901V.capabilities();
        for (displayed, wire) in [(3, 2), (201, 200), (451, 450), (1001, 1000)] {
            let params = user(896_000_000, 906_000_000, displayed);
            assert_eq!(
                CalibrationParams::S11User(params).command(&caps).unwrap(),
                format!("$cal_user_s11,901000000,10000000,{wire}\n")
            );
            assert_eq!(
                CalibrationParams::S21User(params).command(&caps).unwrap(),
                format!("$cal_user_s21,901000000,10000000,{wire}\n")
            );
            assert_eq!(params.points, displayed);
        }
    }

    #[test]
    fn odd_span_round_trip_preserves_endpoints_without_floating_point() {
        for (start, stop) in [
            (5_000, 6_001),
            (0, 1_001),
            (100, 101),
            (u64::MAX - 1_001, u64::MAX),
        ] {
            let params = user(start, stop, 201);
            assert_eq!(params.center_hz, start + (stop - start) / 2);
            assert_eq!(params.endpoints().unwrap(), (start, stop));
        }
    }

    #[test]
    fn invalid_endpoint_arithmetic_is_rejected() {
        assert!(UserCalibrationParams::from_range(2, 1, 201, Rbw::R10k).is_err());
        assert!(UserCalibrationParams::from_range(1, 1, 201, Rbw::R10k).is_err());
        let mut params = user(1_000, 2_000, 201);
        params.center_hz = 0;
        assert!(params.endpoints().is_err());
        params.center_hz = u64::MAX;
        assert!(params.endpoints().is_err());
    }

    #[test]
    fn source_based_user_envelopes_are_mode_specific() {
        let caps = Model::Kc901V.capabilities();
        for (start, stop) in [(5_000, 6_000), (6_999_999_000, 7_000_000_000)] {
            assert!(
                CalibrationParams::S11User(user(start, stop, 201))
                    .validate(&caps)
                    .is_ok()
            );
        }
        assert!(
            CalibrationParams::S21User(user(0, 1_000, 201))
                .validate(&caps)
                .is_ok()
        );
        for (start, stop) in [
            (0, 1_000),
            (4_999, 6_000),
            (5_000, 5_999),
            (6_999_999_000, 7_000_000_001),
        ] {
            assert!(
                CalibrationParams::S11User(user(start, stop, 201))
                    .validate(&caps)
                    .is_err()
            );
        }
        assert!(
            CalibrationParams::S21User(user(0, 999, 201))
                .validate(&caps)
                .is_err()
        );
        assert!(
            CalibrationParams::S21User(user(0, 7_000_000_001, 201))
                .validate(&caps)
                .is_err()
        );
    }

    #[test]
    fn invalid_counts_and_bandwidths_are_rejected_before_building() {
        let caps = Model::Kc901V.capabilities();
        for points in [0, 1, 2, 1_002, u32::MAX] {
            assert!(
                CalibrationParams::S11User(user(5_000, 6_000, points))
                    .command(&caps)
                    .is_err()
            );
        }
        for rbw in Rbw::ALL {
            let params = UserCalibrationParams {
                rbw,
                ..user(5_000, 6_000, 201)
            };
            assert_eq!(
                CalibrationParams::S11User(params).validate(&caps).is_ok(),
                caps.rbw_list.contains(&rbw)
            );
        }
    }

    #[test]
    fn zero_span_and_unavailable_mode_are_rejected() {
        let mut caps = Model::Kc901V.capabilities();
        let params = UserCalibrationParams {
            span_hz: 0,
            ..user(5_000, 6_000, 201)
        };
        assert!(CalibrationParams::S11User(params).validate(&caps).is_err());
        caps.s11.enabled = false;
        assert!(CalibrationParams::S11System.validate(&caps).is_err());
        assert!(
            CalibrationParams::S11User(user(5_000, 6_000, 201))
                .validate(&caps)
                .is_err()
        );
    }

    #[test]
    fn system_commands_have_no_user_range_or_count() {
        let mut caps = Model::Kc901V.capabilities();
        caps.s11.range = FreqRange::new(0, 0, 0);
        caps.s21.range = FreqRange::new(0, 0, 0);
        caps.points_min = 9_999;
        caps.points_max = 9_999;
        assert_eq!(
            CalibrationParams::S11System.command(&caps).unwrap(),
            "$cal_s11\n"
        );
        assert_eq!(
            CalibrationParams::S21System.command(&caps).unwrap(),
            "$cal_s21\n"
        );
        assert_eq!(CalibrationParams::S11System.user(), None);
        assert_eq!(CalibrationKind::S11System.user_range(&caps), None);
    }

    #[test]
    fn system_timeout_keeps_all_6801_points() {
        let caps = Model::Kc901V.capabilities();
        for params in [CalibrationParams::S11System, CalibrationParams::S21System] {
            assert_eq!(params.rbw(&caps), Rbw::R1k);
            assert_eq!(
                params.measurement_timeout(&caps),
                Duration::from_millis(693_702)
            );
        }
        let params = UserCalibrationParams {
            rbw: Rbw::R1k,
            ..user(5_000, 6_000, 1_001)
        };
        assert_eq!(
            CalibrationParams::S11User(params).measurement_timeout(&caps),
            Duration::from_millis(102_000)
        );
        assert_eq!(
            CalibrationParams::S21User(user(0, 1_000, 3)).measurement_timeout(&caps),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn non_kc901v_models_are_not_enabled_by_shared_legacy_fields() {
        for model in [
            Model::Kc901V,
            Model::Kc901Sp,
            Model::Kc901Cp,
            Model::Kc901M,
            Model::Kc901Q,
            Model::Kc901K,
            Model::Kc901R,
            Model::Kc901J,
        ] {
            let caps = model.capabilities();
            for params in [
                CalibrationParams::S11System,
                CalibrationParams::S21System,
                CalibrationParams::S11User(user(1_000_000, 2_000_000, 201)),
                CalibrationParams::S21User(user(1_000_000, 2_000_000, 201)),
            ] {
                assert_eq!(params.validate(&caps).is_ok(), model == Model::Kc901V);
            }
        }
    }

    #[test]
    fn kinds_select_only_the_corresponding_receiver() {
        for (kind, init, stop, mode, is_user) in [
            (
                CalibrationKind::S11System,
                commands::S11_INIT,
                commands::S11_STOP,
                StreamMode::S11,
                false,
            ),
            (
                CalibrationKind::S21System,
                commands::S21_INIT,
                commands::S21_STOP,
                StreamMode::S21,
                false,
            ),
            (
                CalibrationKind::S11User,
                commands::S11_INIT,
                commands::S11_STOP,
                StreamMode::S11,
                true,
            ),
            (
                CalibrationKind::S21User,
                commands::S21_INIT,
                commands::S21_STOP,
                StreamMode::S21,
                true,
            ),
        ] {
            assert_eq!(kind.init_command(), init);
            assert_eq!(kind.stop_command(), stop);
            assert_eq!(kind.mode(), mode);
            assert_eq!(kind.is_user(), is_user);
        }
    }

    #[test]
    fn terminal_or_unknown_reports_are_not_active_work() {
        assert_eq!(
            CalibrationReport::default(),
            CalibrationReport {
                kind: None,
                phase: CalibrationPhase::NotStarted
            }
        );
        for phase in [
            CalibrationPhase::NotStarted,
            CalibrationPhase::Completed,
            CalibrationPhase::Cancelled,
            CalibrationPhase::Unknown,
        ] {
            assert!(
                !CalibrationReport {
                    kind: Some(CalibrationKind::S11User),
                    phase
                }
                .is_active()
            );
        }
        for phase in [
            CalibrationPhase::AwaitingConfirmation,
            CalibrationPhase::WarmingUp,
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
            CalibrationPhase::Measuring,
            CalibrationPhase::Processing,
            CalibrationPhase::Saving,
        ] {
            assert!(phase.is_active());
        }
    }
}
