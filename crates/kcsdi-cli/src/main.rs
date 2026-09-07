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
use kcsdi_core::model::{Capabilities, Model, Rbw};
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
    /// Number of sweep points
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
    /// Number of sweep points
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
        Command::Info(args) => info(&args),
        Command::Sweep { mode } => match mode {
            SweepCommand::S11(args) => sweep_s11(&args),
            SweepCommand::Spec(args) => sweep_spec(&args),
        },
    }
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

/// Validate the sweep window against the model capability table.
fn check_sweep(
    caps: &Capabilities,
    range: kcsdi_core::model::FreqRange,
    start: u64,
    stop: u64,
    points: u32,
) -> Result<(), Box<dyn Error>> {
    if !range.contains_sweep(start, stop) {
        return Err(format!(
            "sweep {start}..{stop} Hz outside {} range {}..{} Hz (min span {} Hz)",
            caps.model, range.min_hz, range.max_hz, range.min_span_hz
        )
        .into());
    }
    if !(caps.points_min..=caps.points_max).contains(&points) {
        return Err(format!(
            "points {points} outside {} range {}..{}",
            caps.model, caps.points_min, caps.points_max
        )
        .into());
    }
    Ok(())
}

fn sweep_s11(args: &S11Args) -> Result<(), Box<dyn Error>> {
    let caps = args.model.capabilities();
    if !caps.s11.enabled {
        return Err(format!("{} has no S11 mode", caps.model).into());
    }
    check_sweep(&caps, caps.s11.range, args.start, args.stop, args.points)?;
    if let Some(rbw) = args.rbw
        && !caps.rbw_list.contains(&rbw)
    {
        return Err(format!("RBW {rbw} not supported by {}", caps.model).into());
    }

    let mut dev = Device::<TcpTransport>::connect(&args.conn.host, args.conn.port)?;
    let params = S11Params {
        cal: args.cal,
        format: args.format,
        points: args.points,
        start_hz: args.start,
        stop_hz: args.stop,
        rbw: args.rbw,
    };
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
    check_sweep(&caps, caps.spec.range, args.start, args.stop, args.points)?;
    if !caps.rbw_list.contains(&args.rbw) {
        return Err(format!("RBW {} not supported by {}", args.rbw, caps.model).into());
    }
    if !(caps.spec.ref_min_dbm..=caps.spec.ref_max_dbm).contains(&args.ref_level) {
        return Err(format!(
            "ref level {} dBm outside {} range {}..{} dBm",
            args.ref_level, caps.model, caps.spec.ref_min_dbm, caps.spec.ref_max_dbm
        )
        .into());
    }

    let mut dev = Device::<TcpTransport>::connect(&args.conn.host, args.conn.port)?;
    let params = SpecParams {
        cal: args.cal,
        lo: args.lo,
        points: args.points,
        start_hz: args.start,
        stop_hz: args.stop,
        rbw: args.rbw,
        ref_level_dbm: args.ref_level,
    };
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
