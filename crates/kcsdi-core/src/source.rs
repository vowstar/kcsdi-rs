// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Typed signal-source requests and protocol-level output state.
//!
//! Parameter limits follow sections 3.10, 3.11, 7.7 and 7.8. These are
//! source-based bounds, not hardware-confirmed output levels. RF-port amplitude
//! limits use the common range without assuming a particular RF-board revision.

use crate::commands;
use crate::model::{Capabilities, Model};
use crate::{Error, Result};

/// Discrete RF ASK depths. The documented set does not include 25 percent.
pub const RF_ASK_DEPTHS: &[u32] = &[
    0, 5, 10, 15, 20, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90,
];
pub const AFOUT_MAX_MV: u32 = 3_000;
pub const AF_FM_MAX_HZ: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Rf,
    Af,
}

impl SourceKind {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Rf => "rfsource",
            Self::Af => "afsource",
        }
    }

    /// Ports exposed by the current KC901V source API.
    pub fn ports(self) -> &'static [SourcePort] {
        match self {
            Self::Rf => &[SourcePort::Port1, SourcePort::Port2],
            Self::Af => &[SourcePort::AfOut, SourcePort::Port1, SourcePort::Port2],
        }
    }

    pub fn max_frequency_hz(self) -> u64 {
        match self {
            Self::Rf => 7_000_000_000,
            Self::Af => 2_000_000_000,
        }
    }

    /// Common RF-port range, without an identified RF-board revision.
    /// Frequency-dependent firmware clamping can still produce warnings.
    pub fn amplitude_dbm_range(self) -> (i32, i32) {
        match self {
            Self::Rf => (-30, 10),
            Self::Af => (-81, 10),
        }
    }

    pub fn init_command(self) -> &'static str {
        match self {
            Self::Rf => commands::RF_SOURCE_INIT,
            Self::Af => commands::AF_SOURCE_INIT,
        }
    }

    pub fn stop_command(self) -> &'static str {
        match self {
            Self::Rf => commands::RF_SOURCE_STOP,
            Self::Af => commands::AF_SOURCE_STOP,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePort {
    Port1,
    Port2,
    AfOut,
}

impl SourcePort {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Port1 => "port1",
            Self::Port2 => "port2",
            Self::AfOut => "afout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAmplitude {
    Dbm(i32),
    Millivolts(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    Off,
    Ask {
        frequency_hz: u32,
        depth_percent: u32,
    },
    Fm {
        frequency_hz: u32,
        deviation_hz: u32,
    },
    Pm {
        frequency_hz: u32,
        phase_deg: i32,
    },
}

impl Modulation {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask { .. } => "ask",
            Self::Fm { .. } => "fm",
            Self::Pm { .. } => "pm",
        }
    }

    /// Bounds for an enabled modulation. RF does not support PM.
    /// OFF has no modulation frequency and returns zero for both bounds.
    pub fn frequency_range(self, kind: SourceKind) -> (u32, u32) {
        match (kind, self) {
            (_, Self::Off) => (0, 0),
            (SourceKind::Rf, _) | (_, Self::Ask { .. }) => (0, 3_000),
            (SourceKind::Af, Self::Fm { .. } | Self::Pm { .. }) => (16, 10_000),
        }
    }

    fn fields(self) -> (u32, i64) {
        match self {
            Self::Off => (0, 0),
            Self::Ask {
                frequency_hz,
                depth_percent,
            } => (frequency_hz, i64::from(depth_percent)),
            Self::Fm {
                frequency_hz,
                deviation_hz,
            } => (frequency_hz, i64::from(deviation_hz)),
            Self::Pm {
                frequency_hz,
                phase_deg,
            } => (frequency_hz, i64::from(phase_deg)),
        }
    }
}

/// Maximum RF FM deviation for a carrier inside the supported RF range.
/// The lowest band follows the manual's 10 MHz limit rather than the
/// reference application's conflicting 50 MHz control limit.
pub fn rf_fm_deviation_max_hz(carrier_hz: u64) -> u32 {
    match carrier_hz {
        0..60_000_000 => 10_000_000,
        60_000_000..106_250_000 => 312_500,
        106_250_000..212_500_000 => 625_000,
        212_500_000..425_000_000 => 1_250_000,
        425_000_000..850_000_000 => 2_500_000,
        850_000_000..1_700_000_000 => 5_000_000,
        1_700_000_000..3_400_000_000 => 10_000_000,
        _ => 20_000_000,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceParams {
    pub kind: SourceKind,
    pub port: SourcePort,
    pub frequency_hz: u64,
    pub amplitude: SourceAmplitude,
    pub modulation: Modulation,
}

impl SourceParams {
    /// Validate the currently supported KC901V source request before sending it.
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        if caps.model != Model::Kc901V {
            return Err(Error::InvalidParameter(
                "signal-source control currently supports KC901V only".into(),
            ));
        }
        if !self.kind.ports().contains(&self.port) {
            return Err(Error::InvalidParameter(
                "output port is not supported for this source".into(),
            ));
        }
        if self.frequency_hz > self.kind.max_frequency_hz() {
            return Err(Error::InvalidParameter(format!(
                "source frequency must be between 0 and {} Hz",
                self.kind.max_frequency_hz()
            )));
        }
        self.validate_amplitude()?;
        self.validate_modulation()
    }

    fn validate_amplitude(&self) -> Result<()> {
        match (self.port, self.amplitude) {
            (SourcePort::AfOut, SourceAmplitude::Millivolts(value)) => {
                if value > AFOUT_MAX_MV {
                    return Err(Error::InvalidParameter(format!(
                        "AFOUT amplitude must be between 0 and {AFOUT_MAX_MV} mV VPP"
                    )));
                }
            }
            (SourcePort::Port1 | SourcePort::Port2, SourceAmplitude::Dbm(value)) => {
                let (min, max) = self.kind.amplitude_dbm_range();
                if !(min..=max).contains(&value) {
                    return Err(Error::InvalidParameter(format!(
                        "source amplitude must be between {min} and {max} dBm"
                    )));
                }
            }
            _ => {
                return Err(Error::InvalidParameter(
                    "use mV VPP for AFOUT and dBm for RF ports".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_modulation(&self) -> Result<()> {
        if self.kind == SourceKind::Rf && matches!(self.modulation, Modulation::Pm { .. }) {
            return Err(Error::InvalidParameter(
                "PM modulation is not supported by the RF source".into(),
            ));
        }
        let (frequency, _) = self.modulation.fields();
        let (min, max) = self.modulation.frequency_range(self.kind);
        if !(min..=max).contains(&frequency) {
            return Err(Error::InvalidParameter(format!(
                "modulation frequency must be between {min} and {max} Hz"
            )));
        }
        match self.modulation {
            Modulation::Off => Ok(()),
            Modulation::Ask { depth_percent, .. } => match self.kind {
                SourceKind::Rf if !RF_ASK_DEPTHS.contains(&depth_percent) => {
                    Err(Error::InvalidParameter(
                        "RF ASK depth must be one of the supported discrete percentages".into(),
                    ))
                }
                SourceKind::Af if depth_percent > 100 => Err(Error::InvalidParameter(
                    "AF ASK depth must be between 0 and 100 percent".into(),
                )),
                _ => Ok(()),
            },
            Modulation::Fm { deviation_hz, .. } => {
                let max = match self.kind {
                    SourceKind::Rf => rf_fm_deviation_max_hz(self.frequency_hz),
                    SourceKind::Af => AF_FM_MAX_HZ,
                };
                if deviation_hz > max {
                    return Err(Error::InvalidParameter(format!(
                        "FM deviation must be between 0 and {max} Hz at this carrier frequency"
                    )));
                }
                Ok(())
            }
            Modulation::Pm { phase_deg, .. } => {
                if !(-180..=180).contains(&phase_deg) {
                    return Err(Error::InvalidParameter(
                        "PM phase must be between -180 and 180 degrees".into(),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Build the wire command without validation, like the other command builders.
    /// Callers must validate before sending. FM deviation is always serialized in Hz.
    pub fn command(&self) -> String {
        let amplitude = match self.amplitude {
            SourceAmplitude::Dbm(value) => i64::from(value),
            SourceAmplitude::Millivolts(value) => i64::from(value),
        };
        let (frequency, depth) = self.modulation.fields();
        format!(
            "${},run,{},{},{},{},{},{}\n",
            self.kind.wire_name(),
            self.modulation.wire_name(),
            self.port.wire_name(),
            self.frequency_hz,
            amplitude,
            frequency,
            depth
        )
    }
}

/// Source state inferred from commands and synchronization, not an RF measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceOutputState {
    #[default]
    NotStarted,
    Requested(SourceKind),
    /// A stop command was sent and the protocol was synchronized afterwards.
    StopSent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceWarning {
    AboveMaximum,
    BelowMinimum,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceReport {
    pub state: SourceOutputState,
    pub warning: Option<SourceWarning>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(kind: SourceKind) -> SourceParams {
        SourceParams {
            kind,
            port: SourcePort::Port1,
            frequency_hz: 1_000_000_000,
            amplitude: SourceAmplitude::Dbm(-20),
            modulation: Modulation::Off,
        }
    }

    fn valid(params: SourceParams) -> bool {
        params.validate(&Model::Kc901V.capabilities()).is_ok()
    }

    #[test]
    fn source_support_is_explicitly_limited_to_kc901v() {
        for model in [
            Model::Kc901Sp,
            Model::Kc901Cp,
            Model::Kc901M,
            Model::Kc901Q,
            Model::Kc901K,
            Model::Kc901R,
            Model::Kc901J,
        ] {
            for kind in [SourceKind::Rf, SourceKind::Af] {
                assert!(params(kind).validate(&model.capabilities()).is_err());
            }
        }
        assert!(valid(params(SourceKind::Rf)));
        assert!(valid(params(SourceKind::Af)));
    }

    #[test]
    fn carrier_limits_include_zero_and_the_documented_upper_bound() {
        for kind in [SourceKind::Rf, SourceKind::Af] {
            let request = params(kind);
            for frequency_hz in [0, 1, kind.max_frequency_hz()] {
                assert!(valid(SourceParams {
                    frequency_hz,
                    ..request
                }));
            }
            for frequency_hz in [kind.max_frequency_hz() + 1, u64::MAX] {
                assert!(!valid(SourceParams {
                    frequency_hz,
                    ..request
                }));
            }
        }
    }

    #[test]
    fn amplitudes_use_common_limits_for_each_rf_port() {
        for kind in [SourceKind::Rf, SourceKind::Af] {
            let (min, max) = kind.amplitude_dbm_range();
            for port in [SourcePort::Port1, SourcePort::Port2] {
                let request = SourceParams {
                    port,
                    ..params(kind)
                };
                for level in [min, 0, max] {
                    assert!(valid(SourceParams {
                        amplitude: SourceAmplitude::Dbm(level),
                        ..request
                    }));
                }
                for level in [min - 1, max + 1, i32::MIN, i32::MAX] {
                    assert!(!valid(SourceParams {
                        amplitude: SourceAmplitude::Dbm(level),
                        ..request
                    }));
                }
                assert!(!valid(SourceParams {
                    amplitude: SourceAmplitude::Millivolts(1_000),
                    ..request
                }));
            }
        }
    }

    #[test]
    fn afout_requires_af_mode_and_millivolts() {
        let request = SourceParams {
            port: SourcePort::AfOut,
            amplitude: SourceAmplitude::Millivolts(1_000),
            ..params(SourceKind::Af)
        };
        for level in [0, 1, AFOUT_MAX_MV] {
            assert!(valid(SourceParams {
                amplitude: SourceAmplitude::Millivolts(level),
                ..request
            }));
        }
        for level in [AFOUT_MAX_MV + 1, u32::MAX] {
            assert!(!valid(SourceParams {
                amplitude: SourceAmplitude::Millivolts(level),
                ..request
            }));
        }
        assert!(!valid(SourceParams {
            amplitude: SourceAmplitude::Dbm(0),
            ..request
        }));
        assert!(!valid(SourceParams {
            kind: SourceKind::Rf,
            ..request
        }));
        assert!(valid(SourceParams {
            frequency_hz: 2_000_000_000,
            ..request
        }));
    }

    #[test]
    fn rf_ask_uses_the_exact_discrete_depth_set() {
        for depth_percent in 0..=100 {
            let request = SourceParams {
                modulation: Modulation::Ask {
                    frequency_hz: 1_000,
                    depth_percent,
                },
                ..params(SourceKind::Rf)
            };
            assert_eq!(valid(request), RF_ASK_DEPTHS.contains(&depth_percent));
        }
        assert!(!RF_ASK_DEPTHS.contains(&25));
    }

    #[test]
    fn af_ask_accepts_every_integral_percentage_through_one_hundred() {
        for depth_percent in 0..=101 {
            let request = SourceParams {
                modulation: Modulation::Ask {
                    frequency_hz: 0,
                    depth_percent,
                },
                ..params(SourceKind::Af)
            };
            assert_eq!(valid(request), depth_percent <= 100);
        }
    }

    #[test]
    fn modulation_frequency_bounds_depend_on_kind_and_modulation() {
        for kind in [SourceKind::Rf, SourceKind::Af] {
            for frequency_hz in [0, 3_000, 3_001] {
                assert_eq!(
                    valid(SourceParams {
                        modulation: Modulation::Ask {
                            frequency_hz,
                            depth_percent: 20
                        },
                        ..params(kind)
                    }),
                    frequency_hz <= 3_000
                );
            }
            let (min, max) = if kind == SourceKind::Rf {
                (0, 3_000)
            } else {
                (16, 10_000)
            };
            for frequency_hz in [0, 15, 16, 3_000, 3_001, 10_000, 10_001, u32::MAX] {
                let modulation = Modulation::Fm {
                    frequency_hz,
                    deviation_hz: 100_000,
                };
                assert_eq!(modulation.frequency_range(kind), (min, max));
                assert_eq!(
                    valid(SourceParams {
                        modulation,
                        ..params(kind)
                    }),
                    (min..=max).contains(&frequency_hz)
                );
            }
        }
    }

    #[test]
    fn rf_fm_limits_change_at_each_exact_carrier_boundary() {
        for (start, end, max) in [
            (0, 59_999_999, 10_000_000),
            (60_000_000, 106_249_999, 312_500),
            (106_250_000, 212_499_999, 625_000),
            (212_500_000, 424_999_999, 1_250_000),
            (425_000_000, 849_999_999, 2_500_000),
            (850_000_000, 1_699_999_999, 5_000_000),
            (1_700_000_000, 3_399_999_999, 10_000_000),
            (3_400_000_000, 7_000_000_000, 20_000_000),
        ] {
            for frequency_hz in [start, end] {
                assert_eq!(rf_fm_deviation_max_hz(frequency_hz), max);
                for deviation_hz in [0, max, max + 1] {
                    assert_eq!(
                        valid(SourceParams {
                            frequency_hz,
                            modulation: Modulation::Fm {
                                frequency_hz: 1_000,
                                deviation_hz
                            },
                            ..params(SourceKind::Rf)
                        }),
                        deviation_hz <= max
                    );
                }
            }
        }
    }

    #[test]
    fn af_fm_deviation_is_one_megahertz_in_wire_units() {
        for deviation_hz in [0, AF_FM_MAX_HZ, AF_FM_MAX_HZ + 1, u32::MAX] {
            assert_eq!(
                valid(SourceParams {
                    modulation: Modulation::Fm {
                        frequency_hz: 10_000,
                        deviation_hz
                    },
                    ..params(SourceKind::Af)
                }),
                deviation_hz <= AF_FM_MAX_HZ
            );
        }
    }

    #[test]
    fn pm_is_af_only_with_signed_phase_and_sixteen_hertz_minimum() {
        for phase_deg in [-181, -180, 0, 180, 181] {
            for frequency_hz in [15, 16, 10_000, 10_001] {
                for kind in [SourceKind::Rf, SourceKind::Af] {
                    assert_eq!(
                        valid(SourceParams {
                            modulation: Modulation::Pm {
                                frequency_hz,
                                phase_deg
                            },
                            ..params(kind)
                        }),
                        kind == SourceKind::Af
                            && (-180..=180).contains(&phase_deg)
                            && (16..=10_000).contains(&frequency_hz)
                    );
                }
            }
        }
    }

    #[test]
    fn commands_preserve_field_order_signs_and_integral_units() {
        for (request, expected) in [
            (
                params(SourceKind::Rf),
                "$rfsource,run,off,port1,1000000000,-20,0,0\n",
            ),
            (
                params(SourceKind::Af),
                "$afsource,run,off,port1,1000000000,-20,0,0\n",
            ),
            (
                SourceParams {
                    modulation: Modulation::Ask {
                        frequency_hz: 1_000,
                        depth_percent: 90,
                    },
                    ..params(SourceKind::Rf)
                },
                "$rfsource,run,ask,port1,1000000000,-20,1000,90\n",
            ),
            (
                SourceParams {
                    port: SourcePort::AfOut,
                    frequency_hz: 10_000_000,
                    amplitude: SourceAmplitude::Millivolts(1_300),
                    modulation: Modulation::Ask {
                        frequency_hz: 1_000,
                        depth_percent: 100,
                    },
                    ..params(SourceKind::Af)
                },
                "$afsource,run,ask,afout,10000000,1300,1000,100\n",
            ),
            (
                SourceParams {
                    port: SourcePort::Port2,
                    modulation: Modulation::Fm {
                        frequency_hz: 3_000,
                        deviation_hz: 123_456,
                    },
                    ..params(SourceKind::Rf)
                },
                "$rfsource,run,fm,port2,1000000000,-20,3000,123456\n",
            ),
            (
                SourceParams {
                    modulation: Modulation::Fm {
                        frequency_hz: 16,
                        deviation_hz: 1_000_000,
                    },
                    ..params(SourceKind::Af)
                },
                "$afsource,run,fm,port1,1000000000,-20,16,1000000\n",
            ),
            (
                SourceParams {
                    modulation: Modulation::Pm {
                        frequency_hz: 10_000,
                        phase_deg: -180,
                    },
                    ..params(SourceKind::Af)
                },
                "$afsource,run,pm,port1,1000000000,-20,10000,-180\n",
            ),
        ] {
            assert!(valid(request));
            assert_eq!(request.command(), expected);
            assert_eq!(request.command().trim_end().split(',').count(), 8);
        }
    }

    #[test]
    fn init_stop_and_default_report_are_unambiguous() {
        assert_eq!(SourceKind::Rf.init_command(), "$rfsource,init\n");
        assert_eq!(SourceKind::Rf.stop_command(), "$rfsource,stop\n");
        assert_eq!(SourceKind::Af.init_command(), "$afsource,init\n");
        assert_eq!(SourceKind::Af.stop_command(), "$afsource,stop\n");
        assert_eq!(
            SourceReport::default(),
            SourceReport {
                state: SourceOutputState::NotStarted,
                warning: None,
            }
        );
    }
}
