<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# kcsdi-rs

Control software for KC901 series vector network analyzers and spectrum
analyzers (KC901V and relatives), written in Rust. MIT licensed.

Status: early development. The CLI (`kcsdi`) connects to a KC901 over
TCP/IP, reads identity and status, and runs S11 and spectrum sweeps with
CSV export. The GUI (`kcsdi-gui`, egui) provides a spectrum analyzer view
with live sweeps. Both are verified against a real KC901V (firmware
V1.6.1).

## Features

- TCP/IP transport (single-connection discipline, clean `$local` exit)
- KC901 text protocol: handshake, identity, temperature, voltage
- S11 and spectrum sweeps with CSV export (CLI)
- Touchstone 1.0/2.0 export: `.s1p` in the GUI and CLI, `.s2p` from four
  complete complex CSVs in the CLI
- Spectrum analyzer GUI: live trace plot with cursor-anchored zoom and
  pan, linear/log frequency axis, sweep parameters, RBW and reference
  level control
- S11 GUI: five display modes (Phase, Return Loss, VSWR, Smith chart,
  Impedance) with linear/log frequency axis for cartesian plots
- Selectable impedance traces and simplified Smith reference markers
- English and Simplified Chinese UI with an embedded CJK fallback font
- Shared sweep validation and an offline `kcsdi limits` command
- GUI settings persist across restarts in the platform-standard config
  directory (`~/.config/kcsdi/config.toml` on Linux), overridable via
  `KCSDI_CONFIG_PATH`
- Protocol implementation verified against a real KC901V (firmware V1.6.1)

## Building

Requires a stable Rust toolchain (edition 2024).

```sh
cargo build --workspace
```

## Usage

`192.0.2.10` below is an RFC 5737 example address; substitute your
instrument's IP (shown in its network settings).

```sh
# Identity and status
kcsdi info --host 192.0.2.10 --port 901

# Supported finite-sweep parameters, without connecting
kcsdi limits --model kc901v

# Spectrum sweep to CSV
kcsdi sweep spec --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 --rbw 10k \
    --out spec.csv

# S11 sweep to CSV
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 1000000 --stop 100000000 --points 201 --format loss \
    --out s11.csv
```

KC901V firmware V1.6.1 accepts S11 sweeps from 5 kHz to 7 GHz and
spectrum sweeps from 0 Hz to 7 GHz, with a minimum span of 1 kHz.
These command boundaries were tested on hardware. They do not establish
measurement accuracy across the entire range.

`--points` and the GUI point field specify returned samples, including
both endpoints. KC901V supports 3 to 1001 samples for finite sweeps.
The protocol count is one less and is converted automatically. The
continuous single-frequency command is not exposed as a finite sweep.

Invalid parameters are rejected before sending sweep commands. The GUI
shows the allowed frequency range and disables RUN until invalid settings
are corrected. Unsupported values from older config files remain visible
instead of being silently changed. Frequency input supports whole-Hz
precision. LOG X affects display only and cannot show the 0 Hz sample.

### Touchstone export

In the GUI, run an S11 sweep in Phase, Smith or Impedance, then click
**Export .s1p**. The last completed sweep is saved in full, independent of
zoom and trace visibility. Return Loss and VSWR alone lack phase and
cannot be exported as complex S-parameters.

```sh
# Direct S11 acquisition. A .s1p destination defaults to complex RI data.
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 5000 --stop 100000000 --points 201 --out s11.s1p

# Offline conversion of an existing complex CSV
kcsdi export s1p --input s11.csv --out s11.s1p

# Offline assembly, with all four measured S-parameters supplied explicitly
kcsdi export s2p --s11 s11.csv --s21 s21.csv \
    --s12 s12.csv --s22 s22.csv --out network.s2p
```

Output uses Hz, real/imaginary pairs and a 50 ohm reference. The default
syntax is Touchstone 2.0. Use `--touchstone-version 1` for older readers.
Existing Touchstone files require `--overwrite` in the CLI or confirmation
in the GUI. Full two-port acquisition is not implemented. The `.s2p`
assembler requires compatible measurements on exactly the same frequency
grid and never invents missing parameters. See
[Touchstone export and verification](docs/touchstone.md) for input formats,
limitations and independent validation commands.

## Protocol References

The protocol layer is informed by the KC901 programming manual
(B002-008, 3rd edition, KeXinShe) and cross-checked against the
documented device behavior of the KCSDI application (MEASALL Technology).
See NOTICE for credits.

## License

MIT. See LICENSE and NOTICE.
