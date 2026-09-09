// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Measurement and identity data types.

use crate::error::{Error, Result};
use crate::protocol::{Packet, StreamMode};

/// Parse a numeric body field. A leading `$` (or anything up to it) is
/// stripped first, matching the KCSDI `indexOf("$")` logic (doc 4.2).
pub fn parse_f64(field: &str) -> Result<f64> {
    let s = match field.find('$') {
        Some(i) => &field[i + 1..],
        None => field,
    };
    s.trim()
        .parse::<f64>()
        .map_err(|_| Error::Protocol(format!("bad number: {field:?}")))
}

/// Extract an arrow-banner field of the form `<-Label:value----->`.
///
/// Implements the KCSDI regexes of doc 2.4 without a regex engine: take the
/// text between the label and the closing `>`, then strip trailing `-`
/// padding. Interior `-` (as in `MB-V1.2`) is preserved.
fn arrow_field(line: &str, label: &str) -> Result<String> {
    let start = line
        .find(label)
        .map(|i| i + label.len())
        .ok_or_else(|| Error::Protocol(format!("device packet: missing field {label:?}")))?;
    let rest = &line[start..];
    let end = rest
        .find('>')
        .ok_or_else(|| Error::Protocol(format!("device packet: unterminated field {label:?}")))?;
    Ok(rest[..end].trim_end_matches('-').to_string())
}

/// Extract a word (`\w+`) after a label, e.g. the username.
fn word_field(line: &str, label: &str) -> Result<String> {
    let start = line
        .find(label)
        .map(|i| i + label.len())
        .ok_or_else(|| Error::Protocol(format!("device packet: missing field {label:?}")))?;
    let word: String = line[start..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if word.is_empty() {
        return Err(Error::Protocol(format!(
            "device packet: empty field {label:?}"
        )));
    }
    Ok(word)
}

/// Identity information from the `device` packet (manual 3.2.11, doc 2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub username: String,
    pub software: String,
    pub hardware: String,
    pub serial: String,
    pub copyright: String,
}

impl DeviceInfo {
    /// Parse from a `device` packet. Body rows 1-5 carry the fields; row 0
    /// is the banner. Rows are re-joined at `,` because the copyright line
    /// contains commas.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        if packet.name != "device" {
            return Err(Error::Protocol(format!(
                "expected device packet, got {:?}",
                packet.name
            )));
        }
        let row = |i: usize| {
            packet
                .args
                .get(i)
                .map(|fields| fields.join(","))
                .ok_or_else(|| Error::Protocol(format!("device packet: missing row {i}")))
        };
        Ok(Self {
            username: word_field(&row(1)?, "<-User @ :")?,
            software: arrow_field(&row(2)?, "<-Software ver:")?,
            hardware: arrow_field(&row(3)?, "<-Hardware ver:")?,
            serial: arrow_field(&row(4)?, "<-Serial num:")?,
            copyright: arrow_field(&row(5)?, "<-Copyright:")?,
        })
    }
}

/// Supply voltages from the `voltage` packet (doc 2.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voltage {
    /// External supply voltage (`chargeVoltage` in KCSDI).
    pub external: f64,
    /// Battery voltage.
    pub battery: f64,
}

impl Voltage {
    /// Parse from a `voltage` packet: `[external V, battery V]`.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        let row = packet
            .args
            .first()
            .ok_or_else(|| Error::Protocol("voltage packet: empty body".into()))?;
        let field = |i: usize| {
            row.get(i)
                .ok_or_else(|| Error::Protocol(format!("voltage packet: missing field {i}")))
                .and_then(|f| parse_f64(f))
        };
        Ok(Self {
            external: field(0)?,
            battery: field(1)?,
        })
    }
}

/// One measured point. `values` follows the per-format column layout of
/// doc 4.2 (e.g. `loss`: `[dB]`, `ma`: `[magnitude, phase]`, `z`:
/// `[|Z|, R, X]`, spec: `[dBm]`).
#[derive(Debug, Clone, PartialEq)]
pub struct SweepPoint {
    /// Frequency in Hz as reported by the instrument (authoritative).
    pub freq_hz: f64,
    /// Data columns after the frequency.
    pub values: Vec<f64>,
}

/// A completed measurement stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepData {
    pub mode: StreamMode,
    /// Wire format of the stream (e.g. `ri`, `loss`; empty for spec).
    pub format: String,
    pub points: Vec<SweepPoint>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PacketParser;

    fn packet_from(lines: &[&str]) -> Packet {
        let mut p = PacketParser::new();
        let mut out = None;
        for line in lines {
            if let Some(packet) = p.feed_line(line).unwrap() {
                out = Some(packet);
            }
        }
        out.unwrap()
    }

    #[test]
    fn parse_f64_strips_dollar_and_scientific() {
        assert_eq!(parse_f64("$47.3").unwrap(), 47.3);
        assert_eq!(parse_f64("$0.528e0").unwrap(), 0.528);
        assert_eq!(parse_f64("$-5.147039e-09").unwrap(), -5.147039e-09);
        assert_eq!(parse_f64("12.16").unwrap(), 12.16);
        assert!(parse_f64("$abc").is_err());
    }

    #[test]
    fn device_info_from_manual_example() {
        // Manual 3.2.11 example packet.
        let packet = packet_from(&[
            "$start,device",
            "$<----------------------KC901 network analyzer----------------->",
            "$<-User @ :BBBB567895>",
            "$<-Software ver:V2.0.8------------------------------------------>",
            "$<-Hardware ver:MB-V1.2 RB-V1.1--------------------------------->",
            "$<-Serial num:002015123456-------------------------------------->",
            "$<-Copyright:KeXinShe Co.,Ltd & KeChuang measurement association>",
            "$<-------------------------------------------------------------->",
            "$end",
        ]);
        let info = DeviceInfo::from_packet(&packet).unwrap();
        assert_eq!(info.username, "BBBB567895");
        assert_eq!(info.software, "V2.0.8");
        assert_eq!(info.hardware, "MB-V1.2 RB-V1.1");
        assert_eq!(info.serial, "002015123456");
        assert_eq!(
            info.copyright,
            "KeXinShe Co.,Ltd & KeChuang measurement association"
        );
    }

    #[test]
    fn device_info_rejects_wrong_packet() {
        let packet = packet_from(&["$start,temp", "$47.3", "$end"]);
        assert!(DeviceInfo::from_packet(&packet).is_err());
    }

    #[test]
    fn voltage_from_verified_sample() {
        // Verified on KC901V firmware V1.6.1 (doc 2.5).
        let packet = packet_from(&["$start,voltage", "$12.16,8.03", "$end"]);
        let v = Voltage::from_packet(&packet).unwrap();
        assert_eq!(v.external, 12.16);
        assert_eq!(v.battery, 8.03);
    }
}
