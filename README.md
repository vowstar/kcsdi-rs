<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# kcsdi-rs

[简体中文](README.zh-CN.md)

A Rust GUI and CLI for KC901 network analyzers. Connect over Ethernet, view S11 and spectrum sweeps, and save measurements.

| Measurement | Views and exports |
| --- | --- |
| S11 | Impedance, Smith chart, phase, return loss and VSWR |
| Spectrum | Live trace with linear or logarithmic frequency axis |
| Data | CSV and Touchstone `.s1p`. CLI assembly of `.s2p` from four complex measurements |
| Interface | English and Simplified Chinese. Selectable impedance traces, zoom, pan and fit |

Hardware tests use a KC901V with firmware V1.6.1. Other KC901 models need hardware tests.

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

In the GUI, enter the instrument address and TCP port, then select Connect. The instrument accepts one control connection at a time.

For the CLI, replace `192.0.2.10` with the instrument address. Frequencies use Hz.

```sh
kcsdi info --host 192.0.2.10 --port 901
kcsdi limits --model kc901v
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 5000 --stop 100000000 --points 201 --out antenna.s1p
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

For GUI export, run a sweep in Phase, Smith or Impedance, then select Export .s1p. The file contains the full completed sweep, including hidden traces and points outside the current view. Return loss and VSWR alone lack phase.

The CLI can combine four complex CSV measurements on the same frequency grid:

```sh
kcsdi export s2p --s11 s11.csv --s21 s21.csv \
    --s12 s12.csv --s22 s22.csv --out network.s2p
```

Full two-port acquisition is not implemented. Export defaults to Touchstone 2.0 with a 50 ohm reference. See [Touchstone export](docs/touchstone.md) for input formats and independent validation.

## References and license

Protocol work is informed by the [KC901 programming manual](https://www.measall.com/) (B002-008, third edition, KeXinShe). It is cross-checked against device behavior documented by [KCSDI](https://deepace.net/products/software/kcsdi-software/) from MEASALL Technology. The interface layout also references KCSDI. Thanks to both projects for documenting these instruments.

The Rust code uses the [MIT license](LICENSE). Font credits and source references are in [NOTICE](NOTICE).
