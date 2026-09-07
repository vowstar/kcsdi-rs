// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! kcsdi: command-line interface for KC901 instruments.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use kcsdi_core::commands::{Cal, Format, Lo};
use kcsdi_core::data::SweepData;
use kcsdi_core::device::{Device, S11Params, SpecParams};
use kcsdi_core::model::{Model, Rbw};
use kcsdi_core::protocol::StreamMode;
use kcsdi_core::transport::TcpTransport;

#[derive(Parser)]
#[command(name = "kcsdi", version, about = "KC901 instrument command-line tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show finite-sweep parameter limits without connecting to a device
    Limits {
        #[arg(long, default_value = "kc901v")]
        model: Model,
    },
    /// Query device identity, temperature and voltages
    Info(ConnArgs),
    /// Run a measurement sweep and write the data to CSV
    Sweep {
        #[command(subcommand)]
        mode: SweepCommand,
    },
}

#[derive(Subcommand)]
enum SweepCommand {
    /// S11 reflection sweep (port 1)
    S11(S11Args),
    /// Spectrum analyzer sweep
    Spec(SpecArgs),
}

#[derive(Args)]
struct ConnArgs {
    /// Instrument host (IP address or hostname)
    #[arg(long)]
    host: String,
    /// Instrument TCP port (configured on the instrument, manual uses 901)
    #[arg(long, default_value_t = 901)]
    port: u16,
}

#[derive(Args)]
struct S11Args {
    #[command(flatten)]
    conn: ConnArgs,
    /// Instrument model, used for range validation
    #[arg(long, default_value = "kc901v")]
    model: Model,
    /// Data format returned by the instrument
    #[arg(long, default_value = "loss")]
    format: Format,
    /// Start frequency in Hz
    #[arg(long)]
    start: u64,
    /// Stop frequency in Hz
    #[arg(long)]
    stop: u64,
    /// Returned samples including endpoints (KC901V: 3..1001)
    #[arg(long)]
    points: u32,
    /// Calibration applied to the measurement
    #[arg(long, default_value = "caloff")]
    cal: Cal,
    /// Sampling bandwidth; when given, $bw is pushed before the run
    #[arg(long)]
    rbw: Option<Rbw>,
    /// Output CSV file
    #[arg(long)]
    out: PathBuf,
}

#[derive(Args)]
struct SpecArgs {
    #[command(flatten)]
    conn: ConnArgs,
    /// Instrument model, used for range validation
    #[arg(long, default_value = "kc901v")]
    model: Model,
    /// Start frequency in Hz
    #[arg(long)]
    start: u64,
    /// Stop frequency in Hz
    #[arg(long)]
    stop: u64,
    /// Returned samples including endpoints (KC901V: 3..1001)
    #[arg(long)]
    points: u32,
    /// Sampling bandwidth ($bw)
    #[arg(long, default_value = "10k")]
    rbw: Rbw,
    /// Reference level in dBm ($specref)
    #[arg(long, default_value_t = -10, allow_hyphen_values = true)]
    ref_level: i32,
    /// Amplitude calibration
    #[arg(long, default_value = "caloff")]
    cal: Cal,
    /// Local oscillator side
    #[arg(long, default_value = "highlo")]
    lo: Lo,
    /// Output CSV file
    #[arg(long)]
    out: PathBuf,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Limits { model } => {
            print!("{}", limits(model));
            Ok(())
        }
        Command::Info(args) => info(&args),
        Command::Sweep { mode } => match mode {
            SweepCommand::S11(args) => sweep_s11(&args),
            SweepCommand::Spec(args) => sweep_spec(&args),
        },
    }
}

fn limits(model: Model) -> String {
    use std::fmt::Write;

    let caps = model.capabilities();
    let mut text = format!("Model: {model}\nFinite ascending sweeps, frequencies in Hz\n");
    for (name, range) in [("S11", caps.s11.range), ("SPEC", caps.spec.range)] {
        writeln!(
            text,
            "{name}: start {}..={}, stop {}..={}, minimum span {}",
            range.min_hz,
            range.max_hz - range.min_span_hz,
            range.min_hz + range.min_span_hz,
            range.max_hz,
            range.min_span_hz
        )
        .expect("writing a String cannot fail");
    }
    writeln!(
        text,
        "Samples: {}..={} including endpoints (wire count = samples - {})",
        caps.points_min, caps.points_max, caps.excess_points
    )
    .expect("writing a String cannot fail");
    writeln!(
        text,
        "S11 calibration: {}",
        choices(caps.s11_calibrations())
    )
    .expect("writing a String cannot fail");
    writeln!(text, "S11 format: {}", choices(caps.s11_formats()))
        .expect("writing a String cannot fail");
    writeln!(
        text,
        "SPEC calibration: {}",
        choices(caps.spec_calibrations())
    )
    .expect("writing a String cannot fail");
    writeln!(text, "SPEC LO: lowlo, highlo").expect("writing a String cannot fail");
    writeln!(text, "RBW: {}", choices(caps.rbw_list)).expect("writing a String cannot fail");
    writeln!(
        text,
        "SPEC reference: {}..={} dBm",
        caps.spec.ref_min_dbm, caps.spec.ref_max_dbm
    )
    .expect("writing a String cannot fail");
    text.push_str("Command limits do not establish measurement accuracy. KC901V V1.6.1 is hardware-verified. Other models are table-based.\n");
    text
}

fn choices<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn info(args: &ConnArgs) -> Result<(), Box<dyn Error>> {
    let mut dev = Device::<TcpTransport>::connect(&args.host, args.port)?;
    let info = dev.device_info()?;
    let temp = dev.temperature()?;
    let voltage = dev.voltage()?;
    dev.close();

    println!("Serial:      {}", info.serial);
    println!("User:        {}", info.username);
    println!("Software:    {}", info.software);
    println!("Hardware:    {}", info.hardware);
    println!("Copyright:   {}", info.copyright);
    println!("Temperature: {temp} C");
    println!(
        "Voltage:     external {} V, battery {} V",
        voltage.external, voltage.battery
    );
    Ok(())
}

fn sweep_s11(args: &S11Args) -> Result<(), Box<dyn Error>> {
    let caps = args.model.capabilities();
    let params = S11Params {
        cal: args.cal,
        format: args.format,
        points: args.points,
        start_hz: args.start,
        stop_hz: args.stop,
        rbw: args.rbw,
    };
    params.validate(&caps)?;
    let mut dev =
        Device::<TcpTransport>::connect_with_model(&args.conn.host, args.conn.port, args.model)?;
    let data = dev.sweep_s11(&params)?;
    dev.close();

    write_csv(&args.out, s11_headers(args.format), &data)?;
    println!(
        "{} points written to {}",
        data.points.len(),
        args.out.display()
    );
    Ok(())
}

fn sweep_spec(args: &SpecArgs) -> Result<(), Box<dyn Error>> {
    let caps = args.model.capabilities();
    let params = SpecParams {
        cal: args.cal,
        lo: args.lo,
        points: args.points,
        start_hz: args.start,
        stop_hz: args.stop,
        rbw: args.rbw,
        ref_level_dbm: args.ref_level,
    };
    params.validate(&caps)?;
    let mut dev =
        Device::<TcpTransport>::connect_with_model(&args.conn.host, args.conn.port, args.model)?;
    let data = dev.sweep_spec(&params)?;
    dev.close();

    write_csv(&args.out, &["freq_hz", "level_dbm"], &data)?;
    println!(
        "{} points written to {}",
        data.points.len(),
        args.out.display()
    );
    Ok(())
}

/// CSV header for an S11 sweep, per format column semantics (doc 4.2).
fn s11_headers(format: Format) -> &'static [&'static str] {
    match format {
        Format::Ri => &["freq_hz", "real", "imag"],
        Format::Ma => &["freq_hz", "magnitude", "phase_deg"],
        Format::Vswr => &["freq_hz", "vswr"],
        Format::Z => &["freq_hz", "z_mag_ohm", "resistance_ohm", "reactance_ohm"],
        Format::Loss => &["freq_hz", "loss_db"],
        Format::Delay => &["freq_hz", "delay_s"],
    }
}

fn format_freq(freq_hz: f64) -> String {
    if freq_hz.fract() == 0.0 {
        format!("{}", freq_hz as u64)
    } else {
        freq_hz.to_string()
    }
}

fn write_csv(path: &Path, headers: &[&str], data: &SweepData) -> Result<(), Box<dyn Error>> {
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record(headers)?;
    for point in &data.points {
        let mut record = vec![format_freq(point.freq_hz)];
        record.extend(point.values.iter().map(|v| v.to_string()));
        wtr.write_record(&record)?;
    }
    wtr.flush()?;
    log::info!(
        "wrote {} {} points to {}",
        data.points.len(),
        match data.mode {
            StreamMode::S11 => "s11",
            StreamMode::Spec => "spec",
            other => other.name(),
        },
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_ascii_and_use_mode_ranges_and_sample_counts() {
        let text = limits(Model::Kc901V);
        assert!(text.is_ascii());
        assert!(text.contains("S11: start 5000..=6999999000, stop 6000..=7000000000"));
        assert!(text.contains("SPEC: start 0..=6999999000, stop 1000..=7000000000"));
        assert!(text.contains("Samples: 3..=1001"));
        assert!(text.contains("S11 calibration: calsys, caluser, caloff"));
        assert!(text.contains("RBW: 1k, 3k, 10k, 30k"));
        assert!(!text.contains("100Hz"));
    }

    #[test]
    fn invalid_sweeps_fail_before_connecting_or_creating_a_csv() {
        for (mode, extra, expected) in [
            ("s11", vec!["--start", "0"], "start frequency 0"),
            ("s11", vec!["--cal", "calon"], "S11 calibration calon"),
            ("s11", vec!["--format", "delay"], "S11 format delay"),
            ("s11", vec!["--rbw", "100Hz"], "RBW 100Hz"),
            ("spec", vec!["--cal", "caluser"], "SPEC calibration caluser"),
            ("spec", vec!["--ref-level", "11"], "reference 11"),
            ("spec", vec!["--points", "1002"], "points 1002"),
        ] {
            let mut argv = vec![
                "kcsdi",
                "sweep",
                mode,
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--out",
                "unused.csv",
            ];
            if !extra.contains(&"--start") {
                argv.extend(["--start", "5000"]);
            }
            argv.extend(["--stop", "1000000"]);
            if !extra.contains(&"--points") {
                argv.extend(["--points", "201"]);
            }
            argv.extend(extra);
            let error = run(Cli::try_parse_from(argv).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
