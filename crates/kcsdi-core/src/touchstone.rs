// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Touchstone 1.0/2.0 S-parameter export, using Hz, RI and a 50 ohm reference.
//! See the specification sections Option Line, Two-Port Data Order and
//! Single-Ended Network Parameter Data. No missing S-parameters are inferred.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use crate::data::SweepData;
use crate::protocol::StreamMode;

pub const REFERENCE_OHMS: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Version {
    V1,
    #[default]
    V2,
}

impl Version {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "1.0",
            Self::V2 => "2.0",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("Touchstone export: {0}")]
    InvalidData(String),
    #[error("Touchstone file I/O: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ExportError>;

fn invalid(message: impl Into<String>) -> ExportError {
    ExportError::InvalidData(message.into())
}

/// Fully validated serialized data. Construct before opening a destination.
#[derive(Debug)]
pub struct Document {
    text: String,
    ports: usize,
}

impl Document {
    /// Export one measured reflection coefficient, never a spectrum or a
    /// magnitude-only sweep. Reference impedance is the KC901's 50 ohms.
    pub fn s1p(s11: &SweepData, version: Version) -> Result<Self> {
        Self::build(&[("S11", StreamMode::S11, s11)], version)
    }

    /// All four sweeps must use the same reference and exact frequency grid.
    /// Arguments and serialization both use the legacy order 11,21,12,22.
    pub fn s2p(
        s11: &SweepData,
        s21: &SweepData,
        s12: &SweepData,
        s22: &SweepData,
        version: Version,
    ) -> Result<Self> {
        Self::build(
            &[
                ("S11", StreamMode::S11, s11),
                ("S21", StreamMode::S21, s21),
                ("S12", StreamMode::S12, s12),
                ("S22", StreamMode::S22, s22),
            ],
            version,
        )
    }

    fn build(traces: &[(&str, StreamMode, &SweepData)], version: Version) -> Result<Self> {
        let mut converted = Vec::with_capacity(traces.len());
        for &(name, mode, data) in traces {
            converted.push(convert_trace(name, mode, data)?);
            if data.points.len() != traces[0].2.points.len()
                || data
                    .points
                    .iter()
                    .zip(&traces[0].2.points)
                    .any(|(a, b)| a.freq_hz != b.freq_hz)
            {
                return Err(invalid(format!(
                    "{name} frequency grid differs from S11. No interpolation is performed"
                )));
            }
        }
        let ports = if traces.len() == 1 { 1 } else { 2 };
        let count = traces[0].2.points.len();
        let mut text = String::from("! kcsdi-rs S-parameters. Source calibration is unchanged.\n");
        if version == Version::V2 {
            text.push_str("[Version] 2.0\n");
        }
        text.push_str("# Hz S RI R 50\n");
        if version == Version::V2 {
            writeln!(text, "[Number of Ports] {ports}").unwrap();
            if ports == 2 {
                text.push_str("[Two-Port Data Order] 21_12\n");
            }
            writeln!(text, "[Number of Frequencies] {count}").unwrap();
            text.push_str("[Matrix Format] Full\n[Network Data]\n");
        }
        text.push_str("! Hz ReS11 ImS11");
        if ports == 2 {
            text.push_str(" ReS21 ImS21 ReS12 ImS12 ReS22 ImS22");
        }
        text.push('\n');
        for (i, point) in traces[0].2.points.iter().enumerate() {
            // 17 significant digits preserve f64 values, including actual
            // device-reported frequencies rather than a synthesized grid.
            write!(text, "{:.16e}", point.freq_hz).unwrap();
            for values in &converted {
                let (real, imag) = values[i];
                write!(text, " {real:.16e} {imag:.16e}").unwrap();
            }
            text.push('\n');
        }
        if version == Version::V2 {
            text.push_str("[End]\n");
        }
        Ok(Self { text, ports })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Atomic replacement only when explicitly permitted by the caller.
    /// No-clobber mode also protects against a destination appearing later.
    pub fn save(&self, path: &Path, overwrite: bool) -> Result<()> {
        let expected = if self.ports == 1 { "s1p" } else { "s2p" };
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
        {
            return Err(invalid(format!("expected a .{expected} destination")));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(self.text.as_bytes())?;
        temporary.as_file().sync_all()?;
        let result = if overwrite {
            temporary.persist(path)
        } else {
            temporary.persist_noclobber(path)
        };
        result.map_err(|error| ExportError::Io(error.error))?;
        Ok(())
    }
}

/// Probe exportability using the actual completed packet, not the currently
/// selected UI format or the current sweep controls.
pub fn validate_s1p(data: &SweepData) -> Result<()> {
    convert_trace("S11", StreamMode::S11, data).map(|_| ())
}

fn convert_trace(name: &str, mode: StreamMode, data: &SweepData) -> Result<Vec<(f64, f64)>> {
    if data.mode != mode {
        return Err(invalid(format!(
            "{name} requires {} data, got {}",
            mode.name(),
            data.mode.name()
        )));
    }
    if data.points.is_empty() {
        return Err(invalid(format!("{name} is empty")));
    }
    let reflection = matches!(mode, StreamMode::S11 | StreamMode::S22);
    let columns = match data.format.as_str() {
        "ri" | "ma" => 2,
        "z" if reflection => 3,
        _ => {
            return Err(invalid(format!(
                "{name} format {:?} does not contain a complete complex S-parameter. Use ri, ma, or reflection impedance z",
                data.format
            )));
        }
    };
    let mut previous = None;
    data.points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let fail = |reason: &str| invalid(format!("{name} sample {}: {reason}", index + 1));
            if !point.freq_hz.is_finite() || point.freq_hz < 0.0 {
                return Err(fail("frequency must be finite and non-negative"));
            }
            if previous.is_some_and(|f| point.freq_hz <= f) {
                return Err(fail("frequencies must be strictly increasing"));
            }
            previous = Some(point.freq_hz);
            if point.values.len() != columns || point.values.iter().any(|v| !v.is_finite()) {
                return Err(fail("invalid column count or non-finite value"));
            }
            let v = &point.values;
            let value = match data.format.as_str() {
                "ri" => (v[0], v[1]),
                "ma" => {
                    if v[0] < 0.0 {
                        return Err(fail("magnitude cannot be negative"));
                    }
                    let angle = v[1].rem_euclid(360.0).to_radians();
                    (v[0] * angle.cos(), v[0] * angle.sin())
                }
                "z" => {
                    if v[0] < 0.0 {
                        return Err(fail("impedance magnitude cannot be negative"));
                    }
                    reflection_coefficient(v[1], v[2], REFERENCE_OHMS)
                        .ok_or_else(|| fail("singular or non-finite impedance-to-S conversion"))?
                }
                _ => unreachable!(),
            };
            if !value.0.is_finite() || !value.1.is_finite() {
                return Err(fail("conversion produced a non-finite S-parameter"));
            }
            Ok(value)
        })
        .collect()
}

/// Stable (Z - Z0)/(Z + Z0), also shared by the Smith chart. Scaling and
/// ratio-based complex division avoid squaring large/small impedances.
pub fn reflection_coefficient(r: f64, x: f64, z0: f64) -> Option<(f64, f64)> {
    if !r.is_finite() || !x.is_finite() || !z0.is_finite() || z0 <= 0.0 {
        return None;
    }
    let scale = r.abs().max(x.abs()).max(z0);
    let (r, x, z0) = (r / scale, x / scale, z0 / scale);
    let (a, b, c, d) = (r - z0, x, r + z0, x);
    if c == 0.0 && d == 0.0 {
        return None;
    }
    // Im(gamma) = 2*z0*x / |Z+z0|^2 avoids subtracting nearly equal
    // terms when |Z| is large. Apply it through the scaled division.
    let (real, imag) = if c.abs() >= d.abs() {
        let ratio = d / c;
        let denominator = c + d * ratio;
        (
            (a + b * ratio) / denominator,
            (2.0 * z0 * ratio) / denominator,
        )
    } else {
        let ratio = c / d;
        let denominator = d + c * ratio;
        ((a * ratio + b) / denominator, (2.0 * z0) / denominator)
    };
    (real.is_finite() && imag.is_finite()).then_some((real, imag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::SweepPoint;

    fn sweep(mode: StreamMode, format: &str, values: &[&[f64]]) -> SweepData {
        SweepData {
            mode,
            format: format.into(),
            points: values
                .iter()
                .enumerate()
                .map(|(i, v)| SweepPoint {
                    freq_hz: i as f64 * 1e6,
                    values: v.to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn versions_use_explicit_units_reference_and_two_port_order() {
        let a = sweep(StreamMode::S11, "ri", &[&[0.1, -0.2]]);
        let b = sweep(StreamMode::S21, "ri", &[&[0.3, 0.4]]);
        let c = sweep(StreamMode::S12, "ri", &[&[-0.5, 0.6]]);
        let d = sweep(StreamMode::S22, "ri", &[&[0.7, -0.8]]);
        for version in [Version::V1, Version::V2] {
            let doc = Document::s2p(&a, &b, &c, &d, version).unwrap();
            assert!(doc.as_str().is_ascii());
            assert!(doc.as_str().contains("# Hz S RI R 50\n"));
            assert_eq!(
                doc.as_str().contains("[Two-Port Data Order] 21_12"),
                version == Version::V2
            );
            assert_eq!(doc.as_str().ends_with("[End]\n"), version == Version::V2);
            let line = doc
                .as_str()
                .lines()
                .find(|line| line.starts_with('0'))
                .unwrap();
            let numbers: Vec<f64> = line
                .split_whitespace()
                .map(|n| n.parse().unwrap())
                .collect();
            assert_eq!(numbers, [0.0, 0.1, -0.2, 0.3, 0.4, -0.5, 0.6, 0.7, -0.8]);
        }
    }

    #[test]
    fn converts_degrees_and_impedance_without_clamping_active_data() {
        let ma = sweep(StreamMode::S11, "ma", &[&[2.0, 90.0], &[0.0, -180.0]]);
        let converted = convert_trace("S11", StreamMode::S11, &ma).unwrap();
        assert!(converted[0].0.abs() < 1e-14);
        assert_eq!(converted[0].1, 2.0);
        assert_eq!(converted[1], (0.0, 0.0));
        assert_eq!(reflection_coefficient(50.0, 0.0, 50.0), Some((0.0, 0.0)));
        assert_eq!(reflection_coefficient(0.0, 0.0, 50.0), Some((-1.0, 0.0)));
        assert_eq!(reflection_coefficient(-25.0, 0.0, 50.0), Some((-3.0, 0.0)));
        let (re, im) = reflection_coefficient(0.0, 50.0, 50.0).unwrap();
        assert_eq!((re, im), (0.0, 1.0));
        let (real, imag) = reflection_coefficient(1e300, 1e300, 50.0).unwrap();
        assert_eq!(real, 1.0);
        assert!((imag / 5e-299 - 1.0).abs() < 1e-14);
        assert!(reflection_coefficient(-50.0, 1e-200, 50.0).is_some());
        assert!(reflection_coefficient(-50.0, 0.0, 50.0).is_none());
    }

    #[test]
    fn rejects_incomplete_invalid_and_misaligned_data() {
        for format in ["loss", "vswr", "delay", "", "unknown"] {
            assert!(
                Document::s1p(&sweep(StreamMode::S11, format, &[&[1.0]]), Version::V2).is_err()
            );
        }
        for values in [
            &[f64::NAN, 0.0][..],
            &[0.0, f64::INFINITY],
            &[1.0],
            &[1.0, 2.0, 3.0],
        ] {
            assert!(Document::s1p(&sweep(StreamMode::S11, "ri", &[values]), Version::V2).is_err());
        }
        assert!(
            Document::s1p(&sweep(StreamMode::Spec, "ri", &[&[1.0, 2.0]]), Version::V2).is_err()
        );
        assert!(Document::s1p(&sweep(StreamMode::S11, "ri", &[]), Version::V2).is_err());
        assert!(
            Document::s1p(
                &sweep(StreamMode::S11, "z", &[&[50.0, -50.0, 0.0]]),
                Version::V2
            )
            .is_err()
        );
        assert!(
            Document::s1p(&sweep(StreamMode::S11, "ma", &[&[-1.0, 30.0]]), Version::V2).is_err()
        );
        let a = sweep(StreamMode::S11, "ri", &[&[0.1, 0.2], &[0.3, 0.4]]);
        for frequency in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut bad = a.clone();
            bad.points[1].freq_hz = frequency;
            assert!(Document::s1p(&bad, Version::V2).is_err());
        }
        let mut b = a.clone();
        b.mode = StreamMode::S21;
        let mut c = a.clone();
        c.mode = StreamMode::S12;
        let mut d = a.clone();
        d.mode = StreamMode::S22;
        c.points[1].freq_hz += 1.0;
        assert!(Document::s2p(&a, &b, &c, &d, Version::V2).is_err());
        c.points[1].freq_hz -= 1.0;
        d.points.pop();
        assert!(Document::s2p(&a, &b, &c, &d, Version::V2).is_err());
    }

    #[test]
    fn output_preserves_float_values_and_protects_existing_files() {
        let data = sweep(StreamMode::S11, "ri", &[&[f64::MIN_POSITIVE, f64::MAX]]);
        let doc = Document::s1p(&data, Version::V2).unwrap();
        let line = doc
            .as_str()
            .lines()
            .find(|line| line.starts_with('0'))
            .unwrap();
        let values: Vec<f64> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(values[1..], data.points[0].values);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.s1p");
        std::fs::write(&path, "existing").unwrap();
        assert!(doc.save(&path, false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing");
        doc.save(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc.as_str());
        assert!(doc.save(&path.with_extension("s2p"), true).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn two_port_export_checks_roles_and_requires_complex_transmission() {
        let a = sweep(StreamMode::S11, "ri", &[&[0.1, 0.2]]);
        let b = sweep(StreamMode::S21, "ma", &[&[2.0, 90.0]]);
        let c = sweep(StreamMode::S12, "ri", &[&[0.3, -0.4]]);
        let d = sweep(StreamMode::S22, "z", &[&[50.0, 50.0, 0.0]]);
        assert!(Document::s2p(&a, &b, &c, &d, Version::V2).is_ok());
        assert!(Document::s2p(&a, &c, &b, &d, Version::V2).is_err());
        for format in ["loss", "vswr", "z"] {
            let bad = sweep(StreamMode::S21, format, &[&[50.0, 50.0, 0.0]]);
            assert!(Document::s2p(&a, &bad, &c, &d, Version::V2).is_err());
        }
    }
}
