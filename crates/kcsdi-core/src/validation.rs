// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared CLI, GUI and session preflight checks for sweeps and single points.
//! KC901V S11 and SPEC limits were exercised on firmware V1.6.1 (section 12).
//! S21 validation follows the source-based table in section 7.3.

use std::fmt::Display;

use crate::device::{PointParams, PointSettings, S11Params, S21Params, SpecParams};
use crate::error::{Error, Result};
use crate::model::{Capabilities, FreqRange, Rbw};

impl Capabilities {
    /// Convert requested samples into the device's sweep-count parameter.
    /// Legacy devices return wire count + 1. Wire count 1 is a continuous
    /// single-frequency mode, not a finite sweep (sections 3.4 and 12).
    pub fn wire_points(&self, samples: u32) -> Result<u32> {
        if !(self.points_min..=self.points_max).contains(&samples) {
            return Err(invalid(format!(
                "{} points {samples}, expected {}..={} returned samples",
                self.model, self.points_min, self.points_max
            )));
        }
        samples
            .checked_sub(self.excess_points)
            .filter(|&points| points >= 2)
            .ok_or_else(|| invalid("finite sweeps require a wire count of at least 2"))
    }
}

impl S11Params {
    /// Validate all parameters without connecting to or changing the device.
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        if !caps.s11.enabled {
            return Err(invalid(format!("{} does not support S11", caps.model)));
        }
        validate_range("S11", caps.s11.range, self.start_hz, self.stop_hz, 5)?;
        caps.wire_points(self.points)?;
        choice("S11 calibration", self.cal, caps.s11_calibrations())?;
        choice("S11 format", self.format, caps.s11_formats())?;
        if let Some(rbw) = self.rbw {
            validate_rbw(caps, rbw)?;
        }
        Ok(())
    }
}

impl S21Params {
    /// Validate source-based S21 limits without touching the device.
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        if !caps.s21.enabled {
            return Err(invalid(format!("{} does not support S21", caps.model)));
        }
        validate_range("S21", caps.s21.range, self.start_hz, self.stop_hz, 6)?;
        caps.wire_points(self.points)?;
        choice("S21 calibration", self.cal, caps.s21_calibrations())?;
        choice("S21 format", self.format, caps.s21_formats())?;
        if let Some(rbw) = self.rbw {
            validate_rbw(caps, rbw)?;
        }
        Ok(())
    }
}

impl SpecParams {
    /// Validate all parameters without connecting to or changing the device.
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        if !caps.spec.enabled {
            return Err(invalid(format!("{} does not support SPEC", caps.model)));
        }
        validate_range("SPEC", caps.spec.range, self.start_hz, self.stop_hz, 5)?;
        caps.wire_points(self.points)?;
        choice("SPEC calibration", self.cal, caps.spec_calibrations())?;
        validate_rbw(caps, self.rbw)?;
        validate_spec_reference(caps, self.ref_level_dbm)
    }
}

impl PointParams {
    /// Check source-based mode bounds and receiver settings without applying
    /// finite sweep span or sample-count limits (sections 3.4, 3.5 and 3.8).
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        let mode = self.settings.mode().name().to_ascii_uppercase();
        let (enabled, range) = match self.settings {
            PointSettings::S11 { .. } => (caps.s11.enabled, caps.s11.range),
            PointSettings::S21 { .. } => (caps.s21.enabled, caps.s21.range),
            PointSettings::Spec { .. } => (caps.spec.enabled, caps.spec.range),
        };
        if !enabled {
            return Err(invalid(format!("{} does not support {mode}", caps.model)));
        }
        if !(range.min_hz..=range.max_hz).contains(&self.frequency_hz) {
            return Err(invalid(format!(
                "{mode} point frequency {} Hz, expected {}..={} Hz for {}",
                self.frequency_hz, range.min_hz, range.max_hz, caps.model
            )));
        }
        match self.settings {
            PointSettings::S11 { cal, format, rbw } => {
                choice("S11 calibration", cal, caps.s11_calibrations())?;
                choice("S11 format", format, caps.s11_formats())?;
                if let Some(rbw) = rbw {
                    validate_rbw(caps, rbw)?;
                }
            }
            PointSettings::S21 {
                cal, format, rbw, ..
            } => {
                choice("S21 calibration", cal, caps.s21_calibrations())?;
                choice("S21 format", format, caps.s21_formats())?;
                if let Some(rbw) = rbw {
                    validate_rbw(caps, rbw)?;
                }
            }
            PointSettings::Spec {
                cal,
                rbw,
                ref_level_dbm,
                ..
            } => {
                choice("SPEC calibration", cal, caps.spec_calibrations())?;
                validate_rbw(caps, rbw)?;
                validate_spec_reference(caps, ref_level_dbm)?;
            }
        }
        Ok(())
    }
}

/// Validate the original floating-point input before rounding to whole Hz.
/// Never allow Rust's saturating float-to-integer cast to turn invalid input
/// into an apparently valid zero-frequency SPEC command.
pub fn frequency_hz(value: f64, field: &str) -> Result<u64> {
    if !value.is_finite() || value < 0.0 || value.round() >= u64::MAX as f64 {
        return Err(invalid(format!(
            "{field} frequency {value} Hz must be finite, non-negative and representable"
        )));
    }
    Ok(value.round() as u64)
}

fn validate_range(
    mode: &str,
    range: FreqRange,
    start: u64,
    stop: u64,
    start_parameter: usize,
) -> Result<()> {
    let max_start = range.max_hz.saturating_sub(range.min_span_hz);
    let min_stop = range.min_hz.saturating_add(range.min_span_hz);
    if !(range.min_hz..=max_start).contains(&start) {
        return Err(invalid(format!(
            "{mode} start frequency {start} Hz (run parameter {start_parameter}), expected {}..={max_start} Hz",
            range.min_hz
        )));
    }
    if !(min_stop..=range.max_hz).contains(&stop) {
        return Err(invalid(format!(
            "{mode} stop frequency {stop} Hz (run parameter {}), expected {min_stop}..={} Hz",
            start_parameter + 1,
            range.max_hz
        )));
    }
    if !range.contains_sweep(start, stop) {
        return Err(invalid(format!(
            "{mode} sweep {start}..{stop} Hz must ascend with a span of at least {} Hz",
            range.min_span_hz
        )));
    }
    Ok(())
}

fn validate_rbw(caps: &Capabilities, rbw: Rbw) -> Result<()> {
    choice(&format!("{} RBW", caps.model), rbw, caps.rbw_list)
}

fn validate_spec_reference(caps: &Capabilities, reference: i32) -> Result<()> {
    if !(caps.spec.ref_min_dbm..=caps.spec.ref_max_dbm).contains(&reference) {
        return Err(invalid(format!(
            "SPEC reference {reference} dBm, expected {}..={} dBm for {}",
            caps.spec.ref_min_dbm, caps.spec.ref_max_dbm, caps.model
        )));
    }
    Ok(())
}

fn choice<T: Display + PartialEq>(field: &str, value: T, allowed: &[T]) -> Result<()> {
    if !allowed.contains(&value) {
        let choices = allowed.iter().map(ToString::to_string).collect::<Vec<_>>();
        return Err(invalid(format!(
            "{field} {value}, expected one of: {}",
            choices.join(", ")
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidParameter(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cal, Format, Lo};
    use crate::model::Model;

    fn s11() -> S11Params {
        S11Params {
            cal: Cal::CalUser,
            format: Format::Z,
            points: 201,
            start_hz: 5_000,
            stop_hz: 7_000_000_000,
            rbw: Some(Rbw::R1k),
        }
    }

    fn spec() -> SpecParams {
        SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 201,
            start_hz: 0,
            stop_hz: 7_000_000_000,
            rbw: Rbw::R1k,
            ref_level_dbm: -10,
        }
    }

    fn s21() -> S21Params {
        S21Params {
            cal: Cal::CalUser,
            format: Format::Delay,
            lo: Lo::HighLo,
            points: 201,
            start_hz: 0,
            stop_hz: 7_000_000_000,
            rbw: Some(Rbw::R1k),
        }
    }

    #[test]
    fn point_limits_do_not_apply_finite_span_or_count_rules() {
        let mut caps = Model::Kc901V.capabilities();
        // Point mode sends literal count one, independent of these finite
        // sample-count limits and the legacy endpoint adjustment.
        caps.points_min = 100;
        caps.points_max = 100;
        for (settings, minimum) in [
            (
                PointSettings::S11 {
                    cal: Cal::CalOff,
                    format: Format::Z,
                    rbw: None,
                },
                5_000,
            ),
            (
                PointSettings::S21 {
                    cal: Cal::CalOff,
                    format: Format::Delay,
                    lo: Lo::HighLo,
                    rbw: None,
                },
                0,
            ),
            (
                PointSettings::Spec {
                    cal: Cal::CalOff,
                    lo: Lo::LowLo,
                    rbw: Rbw::R10k,
                    ref_level_dbm: -10,
                },
                0,
            ),
        ] {
            for frequency_hz in [minimum, minimum + 1, 6_999_999_999, 7_000_000_000] {
                assert!(
                    PointParams {
                        settings: settings.clone(),
                        frequency_hz
                    }
                    .validate(&caps)
                    .is_ok()
                );
            }
            for frequency_hz in [7_000_000_001, u64::MAX] {
                assert!(
                    PointParams {
                        settings: settings.clone(),
                        frequency_hz
                    }
                    .validate(&caps)
                    .is_err()
                );
            }
            if minimum > 0 {
                assert!(
                    PointParams {
                        settings,
                        frequency_hz: minimum - 1
                    }
                    .validate(&caps)
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn point_receiver_choices_match_finite_sweep_choices() {
        let formats = [
            Format::Ri,
            Format::Ma,
            Format::Z,
            Format::Loss,
            Format::Vswr,
            Format::Delay,
        ];
        for model in [Model::Kc901V, Model::Kc901K, Model::Kc901R, Model::Kc901J] {
            let caps = model.capabilities();
            for cal in [Cal::CalOff, Cal::CalOn, Cal::CalSys, Cal::CalUser] {
                for format in formats {
                    for rbw in Rbw::ALL {
                        let s11 = S11Params {
                            cal,
                            format,
                            rbw: Some(rbw),
                            start_hz: 1_000_000,
                            stop_hz: 2_000_000,
                            points: 3,
                        };
                        let point = PointParams {
                            settings: PointSettings::S11 {
                                cal,
                                format,
                                rbw: Some(rbw),
                            },
                            frequency_hz: 1_000_000,
                        };
                        assert_eq!(
                            point.validate(&caps).is_ok(),
                            s11.validate(&caps).is_ok(),
                            "{model} {point:?}"
                        );
                        let s21 = S21Params {
                            cal,
                            format,
                            rbw: Some(rbw),
                            lo: Lo::HighLo,
                            start_hz: 1_000_000,
                            stop_hz: 2_000_000,
                            points: 3,
                        };
                        let point = PointParams {
                            settings: PointSettings::S21 {
                                cal,
                                format,
                                lo: Lo::HighLo,
                                rbw: Some(rbw),
                            },
                            frequency_hz: 1_000_000,
                        };
                        assert_eq!(
                            point.validate(&caps).is_ok(),
                            s21.validate(&caps).is_ok(),
                            "{model} {point:?}"
                        );
                    }
                }
                for rbw in Rbw::ALL {
                    for ref_level_dbm in [-51, -50, -10, 10, 11] {
                        let spec = SpecParams {
                            cal,
                            lo: Lo::LowLo,
                            rbw,
                            ref_level_dbm,
                            start_hz: 1_000_000,
                            stop_hz: 2_000_000,
                            points: 3,
                        };
                        let point = PointParams {
                            settings: PointSettings::Spec {
                                cal,
                                lo: Lo::LowLo,
                                rbw,
                                ref_level_dbm,
                            },
                            frequency_hz: 1_000_000,
                        };
                        assert_eq!(
                            point.validate(&caps).is_ok(),
                            spec.validate(&caps).is_ok(),
                            "{model} {point:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn point_disabled_modes_fail_preflight() {
        let mut caps = Model::Kc901V.capabilities();
        caps.s11.enabled = false;
        caps.s21.enabled = false;
        caps.spec.enabled = false;
        for settings in [
            PointSettings::S11 {
                cal: Cal::CalOff,
                format: Format::Loss,
                rbw: None,
            },
            PointSettings::S21 {
                cal: Cal::CalOff,
                format: Format::Loss,
                lo: Lo::HighLo,
                rbw: None,
            },
            PointSettings::Spec {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                rbw: Rbw::R10k,
                ref_level_dbm: -10,
            },
        ] {
            assert!(
                PointParams {
                    settings,
                    frequency_hz: 1_000_000
                }
                .validate(&caps)
                .is_err()
            );
        }
    }

    #[test]
    fn s21_boundaries_follow_the_source_table_and_report_correct_wire_fields() {
        let caps = Model::Kc901V.capabilities();
        for (start, stop, valid) in [
            (0, 1000, true),
            (0, 999, false),
            (0, 7_000_000_000, true),
            (6_999_999_000, 7_000_000_000, true),
            (6_999_999_001, 7_000_000_000, false),
            (0, 7_000_000_001, false),
            (1_000_000, 100_000, false),
        ] {
            let params = S21Params {
                start_hz: start,
                stop_hz: stop,
                ..s21()
            };
            assert_eq!(params.validate(&caps).is_ok(), valid, "{start}..{stop}");
        }
        for (params, field) in [
            (
                S21Params {
                    start_hz: 7_000_000_000,
                    ..s21()
                },
                "parameter 6",
            ),
            (
                S21Params {
                    stop_hz: 7_000_000_001,
                    ..s21()
                },
                "parameter 7",
            ),
        ] {
            assert!(
                params
                    .validate(&caps)
                    .unwrap_err()
                    .to_string()
                    .contains(field)
            );
        }
        for points in [0, 1, 2, 1002, u32::MAX] {
            assert!(S21Params { points, ..s21() }.validate(&caps).is_err());
        }
        for points in [3, 201, 1001] {
            assert!(S21Params { points, ..s21() }.validate(&caps).is_ok());
        }
    }

    #[test]
    fn s21_format_calibration_and_bandwidth_choices_remain_model_specific() {
        let caps = Model::Kc901V.capabilities();
        for format in [
            Format::Ri,
            Format::Ma,
            Format::Loss,
            Format::Delay,
            Format::Z,
            Format::Vswr,
        ] {
            let valid = !matches!(format, Format::Z | Format::Vswr);
            assert_eq!(S21Params { format, ..s21() }.validate(&caps).is_ok(), valid);
        }
        for cal in [Cal::CalSys, Cal::CalUser, Cal::CalOff, Cal::CalOn] {
            assert_eq!(
                S21Params { cal, ..s21() }.validate(&caps).is_ok(),
                cal != Cal::CalOn
            );
        }
        for rbw in Rbw::ALL {
            assert_eq!(
                S21Params {
                    rbw: Some(rbw),
                    ..s21()
                }
                .validate(&caps)
                .is_ok(),
                caps.rbw_list.contains(&rbw)
            );
        }
        for model in [Model::Kc901K, Model::Kc901R, Model::Kc901J] {
            let caps = model.capabilities();
            let mut params = S21Params {
                cal: Cal::CalOff,
                stop_hz: 1_000_000,
                ..s21()
            };
            assert!(params.validate(&caps).is_err());
            assert!(!caps.s21_formats().contains(&Format::Delay));
            params.format = Format::Z;
            assert!(params.validate(&caps).is_err());
            params.format = Format::Loss;
            assert!(params.validate(&caps).is_ok());
            params.cal = Cal::CalSys;
            assert!(params.validate(&caps).is_err());
        }
        let mut caps = caps;
        caps.s21.enabled = false;
        assert!(s21().validate(&caps).is_err());
    }

    #[test]
    fn kc901v_frequency_boundaries_match_hardware() {
        let caps = Model::Kc901V.capabilities();
        for (start, stop, valid) in [
            (0, 1_000_000, false),
            (4_999, 1_000_000, false),
            (5_000, 6_000, true),
            (5_000, 5_999, false),
            (6_999_999_000, 7_000_000_000, true),
            (6_999_999_001, 7_000_000_000, false),
            (5_000, 7_000_000_001, false),
            (1_000_000, 100_000, false),
        ] {
            let mut params = s11();
            params.start_hz = start;
            params.stop_hz = stop;
            assert_eq!(params.validate(&caps).is_ok(), valid, "{start}..{stop}");
        }
        for (start, stop, valid) in [(0, 1000, true), (0, 999, false), (0, 7_000_000_000, true)] {
            let mut params = spec();
            params.start_hz = start;
            params.stop_hz = stop;
            assert_eq!(params.validate(&caps).is_ok(), valid);
        }
    }

    #[test]
    fn displayed_points_convert_once_and_exclude_continuous_mode() {
        let caps = Model::Kc901V.capabilities();
        for points in [0, 1, 2, 1002, 10001, u32::MAX] {
            assert!(caps.wire_points(points).is_err());
        }
        for (points, wire) in [(3, 2), (201, 200), (1001, 1000)] {
            assert_eq!(caps.wire_points(points).unwrap(), wire);
        }
        let caps = Model::Kc901K.capabilities();
        assert_eq!(caps.wire_points(2).unwrap(), 2);
        assert_eq!(caps.wire_points(1000).unwrap(), 1000);
        assert!(caps.wire_points(1001).is_err());
    }

    #[test]
    fn kc901v_rejects_wrong_mode_cal_format_and_rbw() {
        let caps = Model::Kc901V.capabilities();
        for cal in [Cal::CalSys, Cal::CalUser, Cal::CalOff, Cal::CalOn] {
            let mut params = s11();
            params.cal = cal;
            assert_eq!(params.validate(&caps).is_ok(), cal != Cal::CalOn);
            let mut params = spec();
            params.cal = cal;
            assert_eq!(
                params.validate(&caps).is_ok(),
                matches!(cal, Cal::CalOff | Cal::CalOn)
            );
        }
        let mut params = s11();
        params.format = Format::Delay;
        assert!(params.validate(&caps).is_err());
        for rbw in Rbw::ALL {
            let expected = !matches!(rbw, Rbw::R100Hz | Rbw::R300Hz);
            let mut params = s11();
            params.rbw = Some(rbw);
            assert_eq!(params.validate(&caps).is_ok(), expected);
            let mut params = spec();
            params.rbw = rbw;
            assert_eq!(params.validate(&caps).is_ok(), expected);
        }
    }

    #[test]
    fn reference_level_and_disabled_modes_are_checked() {
        let mut caps = Model::Kc901V.capabilities();
        for level in [-51, -50, 10, 11] {
            let mut params = spec();
            params.ref_level_dbm = level;
            assert_eq!(params.validate(&caps).is_ok(), (-50..=10).contains(&level));
        }
        caps.spec.enabled = false;
        caps.s11.enabled = false;
        assert!(spec().validate(&caps).is_err());
        assert!(s11().validate(&caps).is_err());
    }

    #[test]
    fn float_frequencies_are_checked_before_conversion() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.1,
            u64::MAX as f64,
        ] {
            assert!(frequency_hz(value, "start").is_err());
        }
        assert_eq!(frequency_hz(0.0, "start").unwrap(), 0);
        assert_eq!(frequency_hz(5_000.6, "start").unwrap(), 5_001);
    }
}
