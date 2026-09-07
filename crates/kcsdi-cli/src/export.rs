// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Offline conversion of explicitly identified complex S-parameter CSVs.

use std::error::Error;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use kcsdi_core::commands::Format;
use kcsdi_core::data::{SweepData, SweepPoint};
use kcsdi_core::protocol::StreamMode;
use kcsdi_core::touchstone::{Document, Version};

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum TouchstoneVersion {
    #[value(name = "1")]
    V1,
    #[default]
    #[value(name = "2")]
    V2,
}

impl From<TouchstoneVersion> for Version {
    fn from(version: TouchstoneVersion) -> Self {
        match version {
            TouchstoneVersion::V1 => Self::V1,
            TouchstoneVersion::V2 => Self::V2,
        }
    }
}

#[derive(Args)]
pub struct TouchstoneOptions {
    /// Touchstone syntax version (Hz, RI, 50 ohms in both versions)
    #[arg(long, value_enum, default_value = "2")]
    pub touchstone_version: TouchstoneVersion,
    /// Allow atomic replacement of an existing Touchstone file
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Args)]
pub struct Output {
    /// Destination .s1p or .s2p file
    #[arg(long)]
    out: PathBuf,
    #[command(flatten)]
    options: TouchstoneOptions,
}

#[derive(Subcommand)]
pub enum ExportCommand {
    /// Convert a complex S11 CSV to .s1p without connecting to a device
    S1p {
        #[arg(long)]
        input: PathBuf,
        #[command(flatten)]
        output: Output,
    },
    /// Assemble four 50-ohm complex CSVs on an identical grid into .s2p
    S2p {
        #[arg(long)]
        s11: PathBuf,
        #[arg(long)]
        s21: PathBuf,
        #[arg(long)]
        s12: PathBuf,
        #[arg(long)]
        s22: PathBuf,
        #[command(flatten)]
        output: Output,
    },
}

pub fn run(command: ExportCommand) -> Result<(), Box<dyn Error>> {
    let (document, output) = match command {
        ExportCommand::S1p { input, output } => {
            let trace = read_csv(&input, StreamMode::S11)?;
            let doc = Document::s1p(&trace, output.options.touchstone_version.into())?;
            (doc, output)
        }
        ExportCommand::S2p {
            s11,
            s21,
            s12,
            s22,
            output,
        } => {
            let doc = Document::s2p(
                &read_csv(&s11, StreamMode::S11)?,
                &read_csv(&s21, StreamMode::S21)?,
                &read_csv(&s12, StreamMode::S12)?,
                &read_csv(&s22, StreamMode::S22)?,
                output.options.touchstone_version.into(),
            )?;
            (doc, output)
        }
    };
    document.save(&output.out, output.options.overwrite)?;
    println!("Touchstone written to {}", output.out.display());
    Ok(())
}

fn read_csv(path: &Path, mode: StreamMode) -> Result<SweepData, Box<dyn Error>> {
    parse_csv(std::fs::File::open(path)?, mode)
        .map_err(|error| format!("{}: {error}", path.display()).into())
}

/// Column names carry units and representation. Never guess whether a
/// magnitude is linear or dB, or synthesize phase from a scalar trace.
fn parse_csv(reader: impl std::io::Read, mode: StreamMode) -> Result<SweepData, Box<dyn Error>> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(reader);
    let headers = reader.headers()?.clone();
    let names: Vec<_> = headers.iter().collect();
    let format = match names.as_slice() {
        ["freq_hz", "real", "imag"] => "ri",
        ["freq_hz", "magnitude", "phase_deg"] => "ma",
        ["freq_hz", "z_mag_ohm", "resistance_ohm", "reactance_ohm"] => "z",
        _ => return Err("expected complex CSV headers: freq_hz,real,imag or freq_hz,magnitude,phase_deg or freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm".into()),
    };
    let mut points = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record?;
        let values: Vec<f64> = record
            .iter()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|error| format!("record {}: {error}", index + 1))?;
        points.push(SweepPoint {
            freq_hz: values[0],
            values: values[1..].to_vec(),
        });
    }
    Ok(SweepData {
        mode,
        format: format.into(),
        points,
    })
}

pub fn is_s1p(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("s1p"))
}

/// Decide before connecting. Existing CSV defaults stay unchanged, while
/// .s1p defaults to RI so it has both magnitude and phase information.
pub fn sweep_format(
    path: &Path,
    format: Option<Format>,
    overwrite: bool,
) -> Result<Format, Box<dyn Error>> {
    if is_s1p(path) {
        let format = format.unwrap_or(Format::Ri);
        if !matches!(format, Format::Ri | Format::Ma | Format::Z) {
            return Err(".s1p needs complex data. Choose --format ri, ma or z".into());
        }
        if !overwrite && path.try_exists()? {
            return Err(
                "Touchstone destination exists. Choose another path or pass --overwrite".into(),
            );
        }
        Ok(format)
    } else if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csv"))
    {
        Ok(format.unwrap_or(Format::Loss))
    } else {
        Err("S11 sweep output must be .csv or .s1p. Full .s2p requires export s2p with all four S-parameters".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_headers_determine_complex_representation_and_units() {
        for (csv, expected) in [
            ("freq_hz,real,imag\n5000,0.25,-0.5\n", "ri"),
            ("freq_hz,magnitude,phase_deg\n5000,0.5,90\n", "ma"),
            (
                "freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm\n5000,50,50,0\n",
                "z",
            ),
        ] {
            let data = parse_csv(csv.as_bytes(), StreamMode::S11).unwrap();
            assert_eq!(data.format, expected);
            assert_eq!(data.points[0].freq_hz, 5000.0);
            assert!(Document::s1p(&data, Version::V2).is_ok());
        }
        for csv in [
            "freq_hz,loss_db\n5000,10\n",
            "freq_hz,magnitude,phase_rad\n5000,0.5,1\n",
            "freq_hz,real,imag\n5000,0.25\n",
            "freq_hz,real,imag\n5000,0.25,no\n",
        ] {
            assert!(parse_csv(csv.as_bytes(), StreamMode::S11).is_err());
        }
    }

    #[test]
    fn output_defaults_do_not_lose_phase_or_claim_two_ports() {
        assert_eq!(
            sweep_format(Path::new("sweep.S1P"), None, true).unwrap(),
            Format::Ri
        );
        assert_eq!(
            sweep_format(Path::new("sweep.csv"), None, true).unwrap(),
            Format::Loss
        );
        assert!(sweep_format(Path::new("sweep.s1p"), Some(Format::Loss), true).is_err());
        assert!(sweep_format(Path::new("sweep.s2p"), None, true).is_err());
    }
}
