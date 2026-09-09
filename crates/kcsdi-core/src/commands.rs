// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Command builders (protocol doc section 3).
//!
//! Every builder returns bytes that are sent to the instrument exactly as
//! produced. Fixed commands are constants, parameterized commands are
//! functions. Literals follow the manual and the KCSDI concrete forms.

use std::fmt;
use std::str::FromStr;

use crate::model::Rbw;

/// Handshake byte: a single uppercase `C`, no framing (manual 2.4).
pub const HANDSHAKE: &[u8] = b"C";

/// Interrupt the current command (section 1.4).
pub const ABORT: &[u8] = b"\x03";

/// Exit remote-control mode (manual 3.2.16).
pub const LOCAL: &str = "$local\n";

/// Request the identity packet (manual 3.2.11).
pub const DEVICE: &str = "$device\n";

/// Request the internal temperature packet (manual 3.2.13).
pub const TEMP: &str = "$temp\n";

/// Request the voltage packet (manual 3.2.12).
pub const VOLTAGE: &str = "$voltage\n";

/// Request the pressure/altitude packet (manual 3.2.6).
pub const PRESS_GET: &str = "$press,get\n";

/// Request the run-time counters packet (manual 3.2.14).
pub const TIMES: &str = "$times\n";

/// Initialize S11 mode (mandatory before the first run).
pub const S11_INIT: &str = "$s11,init\n";
/// Stop and close S11 mode.
pub const S11_STOP: &str = "$s11,stop\n";
/// Initialize S21 mode.
pub const S21_INIT: &str = "$s21,init\n";
/// Stop and close S21 mode.
pub const S21_STOP: &str = "$s21,stop\n";
/// Initialize S22 mode (KC901K/R/J only).
pub const S22_INIT: &str = "$s22,init\n";
/// Stop and close S22 mode.
pub const S22_STOP: &str = "$s22,stop\n";
/// Initialize S12 mode (KC901K/R/J only).
pub const S12_INIT: &str = "$s12,init\n";
/// Stop and close S12 mode.
pub const S12_STOP: &str = "$s12,stop\n";
/// Initialize spectrum analyzer mode.
pub const SPEC_INIT: &str = "$spec,init\n";
/// Stop and close spectrum analyzer mode.
pub const SPEC_STOP: &str = "$spec,stop\n";
/// Initialize the RF signal source (section 3.10).
pub const RF_SOURCE_INIT: &str = "$rfsource,init\n";
/// Stop and close the RF signal source.
pub const RF_SOURCE_STOP: &str = "$rfsource,stop\n";
/// Initialize the AF signal source (section 3.11).
pub const AF_SOURCE_INIT: &str = "$afsource,init\n";
/// Stop and close the AF signal source.
pub const AF_SOURCE_STOP: &str = "$afsource,stop\n";

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $( $variant:ident => $lit:literal ),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $( $variant ),+
        }

        impl $name {
            /// Wire literal as sent to the instrument.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $lit ),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $lit => Ok(Self::$variant), )+
                    _ => Err(format!(
                        "invalid {}: {s:?} (expected one of: {})",
                        stringify!($name),
                        [$( $lit ),+].join(", ")
                    )),
                }
            }
        }
    };
}

wire_enum! {
    /// Calibration parameter of `run` commands (legacy models, doc 3.4/3.8).
    Cal {
        CalOn => "calon",
        CalOff => "caloff",
        CalSys => "calsys",
        CalUser => "caluser",
    }
}

wire_enum! {
    /// Device-side data format of `run` commands (doc 3.4/3.5).
    Format {
        Ri => "ri",
        Ma => "ma",
        Vswr => "vswr",
        Z => "z",
        Loss => "loss",
        Delay => "delay",
    }
}

wire_enum! {
    /// Sweep definition mode: center/span or start/stop.
    ScanMode {
        CenterSpan => "cs",
        StartStop => "ss",
    }
}

wire_enum! {
    /// Local oscillator side for S21/S12/SPEC (doc 3.5/3.8).
    Lo {
        LowLo => "lowlo",
        HighLo => "highlo",
    }
}

wire_enum! {
    /// Optional receiver port for SPEC on port-switchable models (doc 3.8).
    SpecPort {
        Port1 => "Port1",
        Port2 => "Port2",
    }
}

/// `$bw,<rbw>\n` -- set the sampling bandwidth (manual 3.2.18).
pub fn set_rbw(rbw: Rbw) -> String {
    format!("$bw,{rbw}\n")
}

/// `$specref,<dBm>\n` -- set the spectrum reference level (manual 3.2.15).
pub fn set_spec_ref(dbm: i32) -> String {
    format!("$specref,{dbm}\n")
}

/// Append literal frequency fields shared by all `run` commands. The
/// caller must pass `None` for f2 in single-frequency continuous mode
/// (wire count 1). This helper does not validate parameters (section 3.4).
fn freq_fields(scan: ScanMode, f1: u64, f2: Option<u64>) -> String {
    match f2 {
        Some(f2) => format!(",{scan},{f1},{f2}"),
        None => format!(",{scan},{f1}"),
    }
}

/// `$s11,run,<cal>,<format>,<points>,<cs|ss>,<f1>[,<f2>]\n` (manual 3.3.1).
pub fn s11_run(
    cal: Cal,
    format: Format,
    points: u32,
    scan: ScanMode,
    f1: u64,
    f2: Option<u64>,
) -> String {
    format!(
        "$s11,run,{cal},{format},{points}{}\n",
        freq_fields(scan, f1, f2)
    )
}

/// `$s21,run,<cal>,<format>,<lo>,<points>,<cs|ss>,<f1>[,<f2>]\n` (manual 3.3.2).
pub fn s21_run(
    cal: Cal,
    format: Format,
    lo: Lo,
    points: u32,
    scan: ScanMode,
    f1: u64,
    f2: Option<u64>,
) -> String {
    format!(
        "$s21,run,{cal},{format},{lo},{points}{}\n",
        freq_fields(scan, f1, f2)
    )
}

/// `$s22,run,<cal>,<format>,<points>,ss,<f1>,<f2>\n` (KC901K/R/J, doc 3.6).
pub fn s22_run(
    cal: Cal,
    format: Format,
    points: u32,
    scan: ScanMode,
    f1: u64,
    f2: Option<u64>,
) -> String {
    format!(
        "$s22,run,{cal},{format},{points}{}\n",
        freq_fields(scan, f1, f2)
    )
}

/// `$s12,run,<cal>,<format>,<lo>,<points>,ss,<f1>,<f2>\n` (KC901K/R/J, doc 3.7).
pub fn s12_run(
    cal: Cal,
    format: Format,
    lo: Lo,
    points: u32,
    scan: ScanMode,
    f1: u64,
    f2: Option<u64>,
) -> String {
    format!(
        "$s12,run,{cal},{format},{lo},{points}{}\n",
        freq_fields(scan, f1, f2)
    )
}

/// `$spec,run,<cal>,<lo>,<points>,<cs|ss>,<f1>[,<f2>][,<port>]\n` (manual 3.3.3).
///
/// The trailing port is only sent on port-switchable models (KC901K/R/J).
pub fn spec_run(
    cal: Cal,
    lo: Lo,
    points: u32,
    scan: ScanMode,
    f1: u64,
    f2: Option<u64>,
    port: Option<SpecPort>,
) -> String {
    let port = match port {
        Some(p) => format!(",{p}"),
        None => String::new(),
    };
    format!(
        "$spec,run,{cal},{lo},{points}{}{port}\n",
        freq_fields(scan, f1, f2)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_command_literals() {
        assert_eq!(HANDSHAKE, b"C");
        assert_eq!(LOCAL, "$local\n");
        assert_eq!(DEVICE, "$device\n");
        assert_eq!(TEMP, "$temp\n");
        assert_eq!(VOLTAGE, "$voltage\n");
        assert_eq!(PRESS_GET, "$press,get\n");
        assert_eq!(TIMES, "$times\n");
        assert_eq!(S11_INIT, "$s11,init\n");
        assert_eq!(S11_STOP, "$s11,stop\n");
        assert_eq!(SPEC_INIT, "$spec,init\n");
        assert_eq!(SPEC_STOP, "$spec,stop\n");
    }

    #[test]
    fn s11_run_matches_manual_example() {
        // Manual 3.3.1 worked example command line.
        assert_eq!(
            s11_run(
                Cal::CalOff,
                Format::Ri,
                2,
                ScanMode::CenterSpan,
                100_000_000,
                Some(50_000_000)
            ),
            "$s11,run,caloff,ri,2,cs,100000000,50000000\n"
        );
    }

    #[test]
    fn s11_run_start_stop_form() {
        assert_eq!(
            s11_run(
                Cal::CalUser,
                Format::Loss,
                201,
                ScanMode::StartStop,
                1_000_000,
                Some(100_000_000)
            ),
            "$s11,run,caluser,loss,201,ss,1000000,100000000\n"
        );
    }

    #[test]
    fn s11_run_single_point_omits_f2() {
        // Manual 3.3.1: with points = 1 only f1 is sent.
        assert_eq!(
            s11_run(
                Cal::CalOff,
                Format::Ri,
                1,
                ScanMode::CenterSpan,
                100_000_000,
                None
            ),
            "$s11,run,caloff,ri,1,cs,100000000\n"
        );
    }

    #[test]
    fn s21_run_matches_manual_example() {
        assert_eq!(
            s21_run(
                Cal::CalOff,
                Format::Ma,
                Lo::LowLo,
                2,
                ScanMode::CenterSpan,
                100_000_000,
                Some(50_000_000)
            ),
            "$s21,run,caloff,ma,lowlo,2,cs,100000000,50000000\n"
        );
    }

    #[test]
    fn spec_run_matches_manual_example() {
        assert_eq!(
            spec_run(
                Cal::CalOff,
                Lo::LowLo,
                10,
                ScanMode::CenterSpan,
                100_000_000,
                Some(50_000_000),
                None
            ),
            "$spec,run,caloff,lowlo,10,cs,100000000,50000000\n"
        );
    }

    #[test]
    fn spec_run_with_optional_port() {
        assert_eq!(
            spec_run(
                Cal::CalOn,
                Lo::HighLo,
                101,
                ScanMode::StartStop,
                1_000_000,
                Some(2_000_000),
                Some(SpecPort::Port1)
            ),
            "$spec,run,calon,highlo,101,ss,1000000,2000000,Port1\n"
        );
    }

    #[test]
    fn settings_commands() {
        assert_eq!(set_rbw(Rbw::R10k), "$bw,10k\n");
        assert_eq!(set_rbw(Rbw::R100Hz), "$bw,100Hz\n");
        assert_eq!(set_spec_ref(-10), "$specref,-10\n");
        assert_eq!(set_spec_ref(10), "$specref,10\n");
    }

    #[test]
    fn enum_parsing_roundtrip() {
        assert_eq!("caloff".parse::<Cal>(), Ok(Cal::CalOff));
        assert_eq!("calsys".parse::<Cal>(), Ok(Cal::CalSys));
        assert_eq!("loss".parse::<Format>(), Ok(Format::Loss));
        assert_eq!("ss".parse::<ScanMode>(), Ok(ScanMode::StartStop));
        assert_eq!("highlo".parse::<Lo>(), Ok(Lo::HighLo));
        assert_eq!("Port2".parse::<SpecPort>(), Ok(SpecPort::Port2));
        assert!("bogus".parse::<Cal>().is_err());
        assert!("port1".parse::<SpecPort>().is_err());
    }
}
