<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# kcsdi-rs

[简体中文](README.zh-CN.md)

A Rust GUI and CLI for KC901 network analyzers. Connect over Ethernet or USB serial, view S11, S21 and spectrum sweeps, and save measurements.

![English impedance and Smith views of one recorded S11 sweep](https://github.com/user-attachments/assets/8eee9271-257a-42d9-8f8b-7aabc8dfcf17)

| Measurement | Views and exports |
| --- | --- |
| S11 | Impedance, Smith chart, phase, return loss and VSWR |
| S21 | Phase, loss and group delay |
| Spectrum | Live trace with linear or logarithmic frequency axis |
| Data | CSV, XLSX and Touchstone `.s1p`. CLI assembly of `.s2p` from four complex measurements |
| Analysis | Selectable traces, hold, maximum and minimum envelopes, markers, zoom, pan and fit |
| Interface | English and Simplified Chinese. Saved devices, light, dark and system themes |

S11 and spectrum hardware tests use a KC901V with firmware V1.6.1. S21, serial and LAN discovery have software and local replay tests. Other KC901 models need hardware tests.

## Build and connect

Install stable Rust. On Ubuntu, install the build libraries first:

```sh
sudo apt-get install build-essential pkg-config libwayland-dev \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libxkbcommon-dev libssl-dev
```

Build both applications, then start the GUI:

```sh
cargo build --release --workspace --locked
cargo run --release -p kcsdi-gui
```

The binaries are in `target/release`. Add that directory to PATH to use the CLI commands that follow.

In the GUI, add a device with its TCP address and port or serial path, open it, then select Connect in the bottom bar. Discover scans for KC901V advertisements and lets you add a saved device. The instrument accepts one control connection at a time.

Add up to ten traces in the left panel and choose their views and colors. Run acquires the visible traces in sequence and shares matching measurements. Each trace keeps its last complete sweep and Y scale. Add markers on the right, then drag them on the chart. Press F11 to toggle fullscreen.

Language follows the system by default. Choose English, Simplified Chinese or Follow system in Settings. Other system languages use English.

For the CLI, replace `192.0.2.10` with the instrument address. Frequencies use Hz.

```sh
kcsdi info --host 192.0.2.10 --port 901
kcsdi serial-ports
kcsdi info --serial /dev/ttyUSB0
kcsdi discover
kcsdi limits --model kc901v
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 5000 --stop 100000000 --points 201 --out antenna.s1p
kcsdi sweep s21 --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 \
    --format delay --rbw 10k --out delay.csv
kcsdi sweep spec --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 --rbw 10k --out spectrum.csv
```

The KC901V accepts these sweep commands with firmware V1.6.1:

| Mode | Frequency range | Minimum span | Points |
| --- | --- | --- | --- |
| S11 | 5 kHz to 7 GHz | 1 kHz | 3 to 1001 |
| Spectrum | 0 Hz to 7 GHz | 1 kHz | 3 to 1001 |

These are tested command limits. Use the instrument specifications for measurement accuracy. LOG X changes the display scale and leaves the sweep spacing unchanged.

## Export

Choose CSV or XLSX to export the selected trace or all visible traces. Files contain complete measurements and their captured settings. Hold, Max and Min are excluded. GUI CSV has one row per raw value with frequency, unit and trace metadata. XLSX has one data sheet per trace and a Metadata sheet.

For `.s1p`, choose Touchstone after an S11 Phase, Smith or Impedance sweep. It saves all samples and complex components of the selected completed trace. Return loss and VSWR alone lack phase.

S21 CLI exports use CSV. Phase uses degrees and group delay uses seconds. Loss values keep the sign reported by the instrument.

The CLI can combine four complex CSV measurements on the same frequency grid:

```sh
kcsdi export s2p --s11 s11.csv --s21 s21.csv \
    --s12 s12.csv --s22 s22.csv --out network.s2p
```

Full two-port acquisition is not implemented. CLI Touchstone export defaults to version 2.0 with a 50 ohm reference. See [Touchstone export](docs/touchstone.md) for input formats and independent validation.

## References and license

Protocol work is informed by the [KC901 programming manual](https://www.measall.com/) (B002-008, third edition, KeXinShe). It is cross-checked against device behavior documented by [KCSDI](https://deepace.net/products/software/kcsdi-software/) from MEASALL Technology. The interface layout also references KCSDI. Thanks to both projects for documenting these instruments.

The Rust code uses the [MIT license](LICENSE). Font credits and source references are in [NOTICE](NOTICE).
