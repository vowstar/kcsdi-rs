// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Raw measurement columns and units (section 4.2).
//! These schemas describe data representation, not model capabilities.

use crate::protocol::StreamMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub unit: &'static str,
}

const fn column(name: &'static str, unit: &'static str) -> Column {
    Column { name, unit }
}

const RI: [Column; 2] = [column("real", ""), column("imag", "")];
const MA: [Column; 2] = [column("magnitude", ""), column("phase_deg", "deg")];
const VSWR: [Column; 1] = [column("vswr", "")];
const IMPEDANCE: [Column; 3] = [
    column("z_mag_ohm", "ohm"),
    column("resistance_ohm", "ohm"),
    column("reactance_ohm", "ohm"),
];
const LOSS: [Column; 1] = [column("loss_db", "dB")];
const DELAY: [Column; 1] = [column("delay_s", "s")];
const SPECTRUM: [Column; 1] = [column("level_dbm", "dBm")];

/// Columns following `freq_hz`, preserving all raw values and their units.
/// Unknown formats and transmission impedance without a topology are rejected.
pub fn columns(mode: StreamMode, format: &str) -> Option<&'static [Column]> {
    use StreamMode::{S11, S12, S21, S22, Spec};
    match (mode, format) {
        (S11 | S21 | S12 | S22, "ri") => Some(&RI),
        (S11 | S21 | S12 | S22, "ma") => Some(&MA),
        (S11 | S21 | S12 | S22, "vswr") => Some(&VSWR),
        (S11 | S21 | S12 | S22, "loss") => Some(&LOSS),
        (S11 | S22, "z") => Some(&IMPEDANCE),
        (S21 | S12, "delay") => Some(&DELAY),
        (Spec, "") => Some(&SPECTRUM),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_column_names_and_units_keep_the_existing_csv_contract() {
        for (mode, format, expected) in [
            (StreamMode::S11, "ri", vec![("real", ""), ("imag", "")]),
            (
                StreamMode::S21,
                "ma",
                vec![("magnitude", ""), ("phase_deg", "deg")],
            ),
            (
                StreamMode::S11,
                "z",
                vec![
                    ("z_mag_ohm", "ohm"),
                    ("resistance_ohm", "ohm"),
                    ("reactance_ohm", "ohm"),
                ],
            ),
            (StreamMode::S22, "vswr", vec![("vswr", "")]),
            (StreamMode::S12, "loss", vec![("loss_db", "dB")]),
            (StreamMode::S21, "delay", vec![("delay_s", "s")]),
            (StreamMode::Spec, "", vec![("level_dbm", "dBm")]),
        ] {
            let columns = columns(mode, format).unwrap();
            assert_eq!(
                columns
                    .iter()
                    .map(|column| (column.name, column.unit))
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(columns.iter().all(|column| column.name != "freq_hz"));
        }
    }

    #[test]
    fn reflection_and_transmission_schemas_are_not_interchangeable() {
        for mode in [StreamMode::S11, StreamMode::S22] {
            assert_eq!(columns(mode, "z"), Some(IMPEDANCE.as_slice()));
            assert!(columns(mode, "delay").is_none());
        }
        for mode in [StreamMode::S21, StreamMode::S12] {
            assert_eq!(columns(mode, "delay"), Some(DELAY.as_slice()));
            assert!(columns(mode, "z").is_none());
            assert!(columns(mode, "series_z").is_none());
            assert!(columns(mode, "parallel_z").is_none());
        }
        for mode in [
            StreamMode::S11,
            StreamMode::S21,
            StreamMode::S12,
            StreamMode::S22,
        ] {
            for format in ["ri", "ma", "vswr", "loss"] {
                assert!(columns(mode, format).is_some());
            }
            for format in ["", "unknown", "RI", "ma "] {
                assert!(columns(mode, format).is_none());
            }
        }
        for format in ["ri", "ma", "vswr", "loss", "z", "delay"] {
            assert!(columns(StreamMode::Spec, format).is_none());
        }
    }
}
