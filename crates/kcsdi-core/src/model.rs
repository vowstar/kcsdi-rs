// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Model capability tables (protocol doc section 7).
//!
//! KC901V is fully implemented. The other models are filled from the KCSDI
//! capability table where documented, so extending them later is a data
//! change, not a structural one.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Sampling bandwidth for `$bw` (manual 3.2.18, doc 3.3.1).
///
/// The `100Hz`/`300Hz` literals include the `Hz` suffix exactly; legacy
/// models only accept `1k`/`3k`/`10k`/`30k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rbw {
    R100Hz,
    R300Hz,
    R1k,
    R3k,
    R10k,
    R30k,
}

impl Rbw {
    /// All known RBW values.
    pub const ALL: [Rbw; 6] = [
        Self::R100Hz,
        Self::R300Hz,
        Self::R1k,
        Self::R3k,
        Self::R10k,
        Self::R30k,
    ];

    /// Wire literal as sent to the instrument.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::R100Hz => "100Hz",
            Self::R300Hz => "300Hz",
            Self::R1k => "1k",
            Self::R3k => "3k",
            Self::R10k => "10k",
            Self::R30k => "30k",
        }
    }

    /// Sweep timeout factor in ms per point (protocol doc 8.2).
    pub fn timeout_factor(self) -> f64 {
        match self {
            Self::R100Hz => 776.4,
            Self::R300Hz => 167.2,
            Self::R1k => 68.0,
            Self::R3k => 25.2,
            Self::R10k => 17.6,
            Self::R30k => 14.4,
        }
    }
}

impl fmt::Display for Rbw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Rbw {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|r| r.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                format!("invalid Rbw: {s:?} (expected one of: 100Hz, 300Hz, 1k, 3k, 10k, 30k)")
            })
    }
}

/// Sweep timeout: `ceil(clamp(factor * points * 1.5, 10 s, 1800 s))`
/// (protocol doc 8.2). Unknown RBW falls back to the `30k` factor.
pub fn sweep_timeout(rbw: Option<Rbw>, points: u32) -> Duration {
    let factor = rbw.map_or(Rbw::R30k.timeout_factor(), Rbw::timeout_factor);
    let ms = (factor * f64::from(points) * 1.5).clamp(1e4, 18e5).ceil();
    Duration::from_millis(ms as u64)
}

/// Firmware version, e.g. `V1.6.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let mut parts = s.split('.');
        let mut next = |what: &str| {
            parts
                .next()
                .and_then(|p| p.parse::<u32>().ok())
                .ok_or_else(|| format!("invalid version {s:?}: bad {what}"))
        };
        Ok(Self {
            major: next("major")?,
            minor: next("minor")?,
            patch: next("patch")?,
        })
    }
}

/// A frequency range with a minimum span, in Hz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreqRange {
    pub min_hz: u64,
    pub max_hz: u64,
    pub min_span_hz: u64,
}

impl FreqRange {
    pub const fn new(min_hz: u64, max_hz: u64, min_span_hz: u64) -> Self {
        Self {
            min_hz,
            max_hz,
            min_span_hz,
        }
    }

    /// True when `[start_hz, stop_hz]` is a legal sweep inside this range.
    pub fn contains_sweep(&self, start_hz: u64, stop_hz: u64) -> bool {
        start_hz >= self.min_hz
            && stop_hz <= self.max_hz
            && stop_hz.saturating_sub(start_hz) >= self.min_span_hz
    }
}

/// Per-mode capabilities for the S-parameter modes.
#[derive(Debug, Clone, Copy)]
pub struct ModeCaps {
    pub enabled: bool,
    pub range: FreqRange,
}

impl ModeCaps {
    const fn disabled() -> Self {
        Self {
            enabled: false,
            range: FreqRange::new(0, 0, 0),
        }
    }

    const fn enabled(min_hz: u64, max_hz: u64) -> Self {
        Self {
            enabled: true,
            range: FreqRange::new(min_hz, max_hz, 1_000),
        }
    }
}

/// Spectrum analyzer capabilities.
#[derive(Debug, Clone, Copy)]
pub struct SpecCaps {
    pub enabled: bool,
    pub range: FreqRange,
    pub ref_min_dbm: i32,
    pub ref_max_dbm: i32,
    pub port_switchable: bool,
}

/// Full capability record for one instrument model (protocol doc 7.1).
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub model: Model,
    pub min_version: Version,
    pub sweep: FreqRange,
    pub points_min: u32,
    pub points_max: u32,
    pub excess_points: u32,
    pub system_cal_count: u32,
    pub system_cal_rbw: Rbw,
    pub rbw_list: &'static [Rbw],
    pub impedance_connection_type: bool,
    pub out_att_max_db: u32,
    pub serial_baud: u32,
    pub s11: ModeCaps,
    pub s21: ModeCaps,
    pub s22: ModeCaps,
    pub s12: ModeCaps,
    pub spec: SpecCaps,
}

const LEGACY_RBW: &[Rbw] = &[Rbw::R1k, Rbw::R3k, Rbw::R10k, Rbw::R30k];
const KRJ_RBW: &[Rbw] = &[
    Rbw::R100Hz,
    Rbw::R300Hz,
    Rbw::R1k,
    Rbw::R3k,
    Rbw::R10k,
    Rbw::R30k,
];

/// Instrument models known to the capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Kc901Sp,
    Kc901Cp,
    Kc901M,
    Kc901V,
    Kc901Q,
    Kc901K,
    Kc901R,
    Kc901J,
}

impl Model {
    /// Model string as used on the wire (mDNS `product` field).
    pub fn name(self) -> &'static str {
        match self {
            Self::Kc901Sp => "KC901SP",
            Self::Kc901Cp => "KC901CP",
            Self::Kc901M => "KC901M",
            Self::Kc901V => "KC901V",
            Self::Kc901Q => "KC901Q",
            Self::Kc901K => "KC901K",
            Self::Kc901R => "KC901R",
            Self::Kc901J => "KC901J",
        }
    }

    /// Capability record from protocol doc section 7.
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Kc901V => legacy_caps(
                self,
                Version {
                    major: 1,
                    minor: 5,
                    patch: 9,
                },
                100_000,
                6_800_000_000,
                6_801,
                25,
                ModeCaps::enabled(5_000, 7_000_000_000),
                ModeCaps::enabled(0, 7_000_000_000),
                7_000_000_000,
            ),
            Self::Kc901Sp => legacy_caps(
                self,
                Version {
                    major: 1,
                    minor: 2,
                    patch: 9,
                },
                9_000,
                4_100_000_000,
                4_101,
                20,
                ModeCaps::enabled(5_000, 4_100_000_000),
                ModeCaps::enabled(0, 4_100_000_000),
                4_100_000_000,
            ),
            Self::Kc901Cp => legacy_caps(
                self,
                Version {
                    major: 1,
                    minor: 2,
                    patch: 9,
                },
                9_000,
                2_000_000_000,
                2_001,
                20,
                ModeCaps::enabled(5_000, 2_000_000_000),
                ModeCaps::enabled(0, 2_000_000_000),
                2_000_000_000,
            ),
            Self::Kc901M => legacy_caps(
                self,
                Version {
                    major: 1,
                    minor: 5,
                    patch: 9,
                },
                9_000,
                9_800_000_000,
                9_801,
                25,
                ModeCaps::enabled(9_000, 9_800_000_000),
                ModeCaps::enabled(0, 9_800_000_000),
                9_800_000_000,
            ),
            Self::Kc901Q => legacy_caps(
                self,
                Version {
                    major: 1,
                    minor: 2,
                    patch: 6,
                },
                9_000,
                20_000_000_000,
                20_001,
                25,
                ModeCaps::enabled(5_000, 30_000_000_000),
                ModeCaps::enabled(0, 30_000_000_000),
                30_000_000_000,
            ),
            Self::Kc901K => krj_caps(self, 4_100_000_000, 445),
            Self::Kc901R => krj_caps(self, 9_800_000_000, 1_026),
            Self::Kc901J => krj_caps(self, 2_100_000_000, 445),
        }
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Model {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let m = match s.to_ascii_uppercase().as_str() {
            "KC901SP" => Self::Kc901Sp,
            "KC901CP" => Self::Kc901Cp,
            "KC901M" => Self::Kc901M,
            "KC901V" => Self::Kc901V,
            "KC901Q" => Self::Kc901Q,
            "KC901K" => Self::Kc901K,
            "KC901R" => Self::Kc901R,
            "KC901J" => Self::Kc901J,
            _ => return Err(format!("unknown model: {s:?}")),
        };
        Ok(m)
    }
}

/// Legacy models (SP/CP/M/V/Q): shared shape, per-model numbers from doc 7.
#[allow(clippy::too_many_arguments)]
fn legacy_caps(
    model: Model,
    min_version: Version,
    sweep_min: u64,
    sweep_max: u64,
    sys_cal_count: u32,
    out_att_max_db: u32,
    s11: ModeCaps,
    s21: ModeCaps,
    spec_max: u64,
) -> Capabilities {
    Capabilities {
        model,
        min_version,
        sweep: FreqRange::new(sweep_min, sweep_max, 1_000),
        points_min: 3,
        points_max: 1001,
        excess_points: 1,
        system_cal_count: sys_cal_count,
        system_cal_rbw: Rbw::R1k,
        rbw_list: LEGACY_RBW,
        impedance_connection_type: false,
        out_att_max_db,
        serial_baud: 921_600,
        s11,
        s21,
        s22: ModeCaps::disabled(),
        s12: ModeCaps::disabled(),
        spec: SpecCaps {
            enabled: true,
            range: FreqRange::new(0, spec_max, 1_000),
            ref_min_dbm: -50,
            ref_max_dbm: 10,
            port_switchable: false,
        },
    }
}

/// KC901K/R/J: shared shape from doc 7.1.
fn krj_caps(model: Model, max_hz: u64, sys_cal_count: u32) -> Capabilities {
    Capabilities {
        model,
        min_version: Version {
            major: 1,
            minor: 5,
            patch: 3,
        },
        sweep: FreqRange::new(0, max_hz, 1_000),
        points_min: 2,
        points_max: 1000,
        excess_points: 0,
        system_cal_count: sys_cal_count,
        system_cal_rbw: Rbw::R1k,
        rbw_list: KRJ_RBW,
        impedance_connection_type: true,
        out_att_max_db: 20,
        serial_baud: 460_800,
        s11: ModeCaps::enabled(0, max_hz),
        s21: ModeCaps::enabled(0, max_hz),
        s22: ModeCaps::enabled(0, max_hz),
        s12: ModeCaps::enabled(0, max_hz),
        spec: SpecCaps {
            enabled: true,
            range: FreqRange::new(0, max_hz, 1_000),
            ref_min_dbm: -200,
            ref_max_dbm: 100,
            port_switchable: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_timeout_formula() {
        // 17.6 * 1000 * 1.5 = 26400 ms.
        assert_eq!(
            sweep_timeout(Some(Rbw::R10k), 1000),
            Duration::from_millis(26_400)
        );
        // 14.4 * 3 * 1.5 = 64.8 ms, clamped to 10 s.
        assert_eq!(sweep_timeout(Some(Rbw::R30k), 3), Duration::from_secs(10));
        // 776.4 * 2000 * 1.5 = 2329200 ms, clamped to 1800 s.
        assert_eq!(
            sweep_timeout(Some(Rbw::R100Hz), 2000),
            Duration::from_secs(1800)
        );
        // Unknown RBW falls back to the 30k factor.
        assert_eq!(sweep_timeout(None, 1000), Duration::from_millis(21_600));
    }

    #[test]
    fn rbw_literals_and_parsing() {
        assert_eq!(Rbw::R100Hz.as_str(), "100Hz");
        assert_eq!("10k".parse::<Rbw>(), Ok(Rbw::R10k));
        assert_eq!("100Hz".parse::<Rbw>(), Ok(Rbw::R100Hz));
        assert!("5k".parse::<Rbw>().is_err());
    }

    #[test]
    fn version_parsing_and_ordering() {
        let v: Version = "V1.6.1".parse().unwrap();
        assert_eq!(
            v,
            Version {
                major: 1,
                minor: 6,
                patch: 1
            }
        );
        // KCSDI-style comparison: 1.6.1 > 1.5.9.
        let min: Version = "1.5.9".parse().unwrap();
        assert!(v > min);
        assert!("1.6".parse::<Version>().is_err());
    }

    #[test]
    fn kc901v_capabilities() {
        let caps = Model::Kc901V.capabilities();
        assert_eq!(caps.model.name(), "KC901V");
        assert_eq!(caps.sweep, FreqRange::new(100_000, 6_800_000_000, 1_000));
        assert_eq!((caps.points_min, caps.points_max), (3, 1001));
        assert_eq!(caps.excess_points, 1);
        assert_eq!(caps.system_cal_count, 6801);
        assert_eq!(caps.rbw_list, LEGACY_RBW);
        assert_eq!(caps.serial_baud, 921_600);
        assert_eq!(caps.out_att_max_db, 25);
        assert!(caps.s11.enabled);
        assert_eq!(caps.s11.range.max_hz, 7_000_000_000);
        assert!(caps.s21.enabled);
        assert!(!caps.s22.enabled);
        assert!(!caps.s12.enabled);
        assert_eq!((caps.spec.ref_min_dbm, caps.spec.ref_max_dbm), (-50, 10));
        assert!(!caps.spec.port_switchable);
    }

    #[test]
    fn model_parsing() {
        assert_eq!("kc901v".parse::<Model>(), Ok(Model::Kc901V));
        assert_eq!("KC901Q".parse::<Model>(), Ok(Model::Kc901Q));
        assert!("kc900".parse::<Model>().is_err());
    }

    #[test]
    fn freq_range_sweep_check() {
        let range = FreqRange::new(100_000, 6_800_000_000, 1_000);
        assert!(range.contains_sweep(1_000_000, 100_000_000));
        assert!(!range.contains_sweep(1_000, 100_000_000));
        assert!(!range.contains_sweep(1_000_000, 7_000_000_000));
        assert!(!range.contains_sweep(1_000_000, 1_000_500));
    }
}
