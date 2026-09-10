// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Finite host-controlled signal-source sessions.

use std::error::Error;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use kcsdi_core::Device;
use kcsdi_core::control::CancellationToken;
use kcsdi_core::model::Model;
use kcsdi_core::source::{
    Modulation, SourceAmplitude, SourceKind, SourceParams, SourcePort, SourceWarning,
};
use kcsdi_core::transport::Transport;

use crate::ConnArgs;

#[derive(Subcommand)]
pub enum SourceCommand {
    /// Show KC901V source limits without connecting
    Limits,
    /// Request RF output, then stop when the host duration expires or on Ctrl+C
    Rf(SourceArgs),
    /// Request AF output, then stop when the host duration expires or on Ctrl+C
    Af(SourceArgs),
    /// Send both source stop commands and release remote control
    Stop(ConnArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum Port {
    Port1,
    Port2,
    Afout,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum ModulationKind {
    #[default]
    Off,
    Ask,
    Fm,
    Pm,
}

#[derive(Args)]
pub struct SourceArgs {
    #[command(flatten)]
    conn: ConnArgs,
    /// Output port. Defaults to port1 for RF and afout for AF
    #[arg(long, value_enum)]
    output: Option<Port>,
    /// Carrier frequency in Hz
    #[arg(long)]
    frequency: u64,
    /// Integer dBm for port1 or port2, mV VPP for afout
    #[arg(long, allow_hyphen_values = true)]
    amplitude: i32,
    /// Host hold time after startup, in seconds
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    seconds: u64,
    #[arg(long, value_enum, default_value = "off")]
    modulation: ModulationKind,
    /// Modulation frequency in Hz. Required when modulation is enabled
    #[arg(long)]
    mod_frequency: Option<u32>,
    /// ASK percent, FM deviation in Hz, or PM degrees
    #[arg(long, allow_hyphen_values = true)]
    depth: Option<i32>,
}

impl SourceArgs {
    fn params(&self, kind: SourceKind) -> Result<SourceParams, String> {
        let port = match self.output {
            Some(Port::Port1) => SourcePort::Port1,
            Some(Port::Port2) => SourcePort::Port2,
            Some(Port::Afout) => SourcePort::AfOut,
            None if kind == SourceKind::Rf => SourcePort::Port1,
            None => SourcePort::AfOut,
        };
        let amplitude = if port == SourcePort::AfOut {
            SourceAmplitude::Millivolts(
                self.amplitude
                    .try_into()
                    .map_err(|_| "AFOUT amplitude cannot be negative")?,
            )
        } else {
            SourceAmplitude::Dbm(self.amplitude)
        };
        let modulation = if matches!(self.modulation, ModulationKind::Off) {
            if self.mod_frequency.is_some() || self.depth.is_some() {
                return Err("omit modulation frequency and depth when modulation is off".into());
            }
            Modulation::Off
        } else {
            let frequency_hz = self
                .mod_frequency
                .ok_or("modulation requires --mod-frequency")?;
            let depth = self.depth.ok_or("modulation requires --depth")?;
            match self.modulation {
                ModulationKind::Ask => Modulation::Ask {
                    frequency_hz,
                    depth_percent: depth
                        .try_into()
                        .map_err(|_| "ASK depth cannot be negative")?,
                },
                ModulationKind::Fm => Modulation::Fm {
                    frequency_hz,
                    deviation_hz: depth
                        .try_into()
                        .map_err(|_| "FM deviation cannot be negative")?,
                },
                ModulationKind::Pm => Modulation::Pm {
                    frequency_hz,
                    phase_deg: depth,
                },
                ModulationKind::Off => unreachable!(),
            }
        };
        let params = SourceParams {
            kind,
            port,
            frequency_hz: self.frequency,
            amplitude,
            modulation,
        };
        params
            .validate(&Model::Kc901V.capabilities())
            .map_err(|error| error.to_string())?;
        Ok(params)
    }
}

pub fn run(command: SourceCommand) -> Result<(), Box<dyn Error>> {
    let (args, kind) = match command {
        SourceCommand::Limits => {
            print!("{}", limits());
            return Ok(());
        }
        SourceCommand::Stop(conn) => {
            conn.target()?;
            let cancel = crate::signal_token()?;
            let mut device = conn.connect_controlled(Model::Kc901V, &cancel)?;
            let result = device.stop_source_controlled(&CancellationToken::default());
            device.close();
            drop(device);
            result?;
            println!("Source stop sent. Physical output is not measured.");
            return Ok(());
        }
        SourceCommand::Rf(args) => (args, SourceKind::Rf),
        SourceCommand::Af(args) => (args, SourceKind::Af),
    };
    // No connection or signal handler is installed for invalid settings.
    let params = args.params(kind)?;
    args.conn.target()?;
    let cancel = crate::signal_token()?;
    let mut device = args.conn.connect_controlled(Model::Kc901V, &cancel)?;
    let outcome = run_session(
        &mut device,
        &params,
        Duration::from_secs(args.seconds),
        &cancel,
    );
    // Release remote mode even after a failed stop fence.
    device.close();
    drop(device);
    // Console pipes can block. Report only after stop and remote release.
    report_warning(outcome.warning);
    if outcome.stop_sent {
        println!("Source stop sent. Physical output is not measured.");
    } else {
        eprintln!("WARN: Source output state is unknown. Check the instrument.");
    }
    outcome.result.map_err(Into::into)
}

struct RunOutcome {
    result: kcsdi_core::Result<()>,
    warning: Option<SourceWarning>,
    stop_sent: bool,
}

fn run_session<T: Transport>(
    device: &mut Device<T>,
    params: &SourceParams,
    duration: Duration,
    cancel: &CancellationToken,
) -> RunOutcome {
    let result = (|| {
        device.start_source_controlled(params, cancel)?;
        let started = Instant::now();
        while started.elapsed() < duration && !cancel.is_cancelled() {
            device.poll_source_controlled(cancel)?;
        }
        Ok(())
    })();
    // Cancellation stops the run, not this cleanup operation.
    let stopped = device.stop_source_controlled(&CancellationToken::default());
    let warning = device.source_report().warning;
    let stop_sent = stopped.is_ok();
    let result = match result {
        Ok(()) | Err(kcsdi_core::Error::Cancelled) => stopped.map(|_| ()),
        Err(error) => Err(error),
    };
    RunOutcome {
        result,
        warning,
        stop_sent,
    }
}

fn report_warning(warning: Option<SourceWarning>) {
    if let Some(warning) = warning {
        eprintln!(
            "WARN: {}",
            match warning {
                SourceWarning::AboveMaximum => "Requested amplitude exceeds the output limit.",
                SourceWarning::BelowMinimum => "Requested amplitude is below the output limit.",
            }
        );
    }
}

fn limits() -> String {
    use std::fmt::Write;

    let mut text = String::from("KC901V source limits. Hardware acceptance pending.\n");
    for kind in [SourceKind::Rf, SourceKind::Af] {
        let (min, max) = kind.amplitude_dbm_range();
        writeln!(
            text,
            "{}: 0..={} Hz, RF ports {min}..={max} dBm",
            kind.wire_name(),
            kind.max_frequency_hz()
        )
        .unwrap();
    }
    text.push_str("AFOUT: 0..=3000 mV VPP. RF source uses port1 or port2.\n");
    text.push_str("RF ASK percent: ");
    text.push_str(
        &kcsdi_core::source::RF_ASK_DEPTHS
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );
    text.push_str("\nRF modulation frequency: 0..=3000 Hz.\nRF FM deviation depends on carrier:\n");
    for (start, end) in [
        (0, 59_999_999_u64),
        (60_000_000, 106_249_999),
        (106_250_000, 212_499_999),
        (212_500_000, 424_999_999),
        (425_000_000, 849_999_999),
        (850_000_000, 1_699_999_999),
        (1_700_000_000, 3_399_999_999),
        (3_400_000_000, 7_000_000_000),
    ] {
        writeln!(
            text,
            "  {start}..={end} Hz: 0..={} Hz",
            kcsdi_core::source::rf_fm_deviation_max_hz(start)
        )
        .unwrap();
    }
    text.push_str("AF ASK: 0..=3000 Hz, 0..=100 percent.\nAF FM/PM: 16..=10000 Hz, FM 0..=1000000 Hz, PM -180..=180 deg.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parsed(extra: &[&str]) -> Result<(SourceArgs, SourceKind), String> {
        let cli = crate::Cli::try_parse_from(
            ["kcsdi", "source"].into_iter().chain(extra.iter().copied()),
        )
        .map_err(|e| e.to_string())?;
        match cli.command {
            crate::Command::Source {
                command: SourceCommand::Rf(args),
            } => Ok((args, SourceKind::Rf)),
            crate::Command::Source {
                command: SourceCommand::Af(args),
            } => Ok((args, SourceKind::Af)),
            _ => Err("unexpected command".into()),
        }
    }

    #[test]
    fn source_arguments_require_duration_and_use_explicit_units() {
        let base = [
            "rf",
            "--host",
            "unused.invalid",
            "--frequency",
            "1000000",
            "--amplitude",
            "-30",
        ];
        assert!(parsed(&base).is_err());
        let (args, kind) = parsed(&[base.as_slice(), &["--seconds", "5"]].concat()).unwrap();
        assert_eq!(
            args.params(kind).unwrap().command(),
            "$rfsource,run,off,port1,1000000,-30,0,0\n"
        );
        for seconds in ["0", "86401", "NaN", "1.5"] {
            assert!(parsed(&[base.as_slice(), &["--seconds", seconds]].concat()).is_err());
        }
    }

    #[test]
    fn invalid_source_arguments_are_rejected_without_a_connection() {
        let base = [
            "rf",
            "--host",
            "unused.invalid",
            "--frequency",
            "1000000",
            "--amplitude",
            "-30",
            "--seconds",
            "1",
        ];
        for extra in [
            vec!["--output", "afout"],
            vec![
                "--modulation",
                "pm",
                "--mod-frequency",
                "1000",
                "--depth",
                "20",
            ],
            vec![
                "--modulation",
                "ask",
                "--mod-frequency",
                "1000",
                "--depth",
                "25",
            ],
            vec!["--modulation", "fm"],
            vec!["--depth", "0"],
        ] {
            let (args, kind) = parsed(&[base.as_slice(), &extra].concat()).unwrap();
            assert!(args.params(kind).is_err());
        }
    }

    #[test]
    fn af_amplitude_and_modulation_preserve_negative_phase_and_hz() {
        for (modulation, depth, expected) in [
            (
                "pm",
                "-180",
                "$afsource,run,pm,afout,10000000,1300,1000,-180\n",
            ),
            (
                "fm",
                "1000000",
                "$afsource,run,fm,afout,10000000,1300,1000,1000000\n",
            ),
        ] {
            let (args, kind) = parsed(&[
                "af",
                "--host",
                "unused.invalid",
                "--frequency",
                "10000000",
                "--amplitude",
                "1300",
                "--seconds",
                "2",
                "--modulation",
                modulation,
                "--mod-frequency",
                "1000",
                "--depth",
                depth,
            ])
            .unwrap();
            assert_eq!(args.params(kind).unwrap().command(), expected);
        }
        assert!(limits().is_ascii());
        assert!(limits().contains("Hardware acceptance pending"));
    }
}
