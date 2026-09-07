# kcsdi-rs

Control software for KC901 series vector network analyzers and spectrum
analyzers (KC901V and relatives), written in Rust. MIT licensed.

Status: early development. The CLI (`kcsdi`) connects to a KC901 over
TCP/IP, reads identity and status, and runs S11 and spectrum sweeps with
CSV export. An egui GUI is planned next.

## Features

- TCP/IP transport (single-connection discipline, clean `$local` exit)
- KC901 text protocol: handshake, identity, temperature, voltage
- S11 and spectrum sweeps with CSV export
- Protocol implementation verified against a real
  KC901V (firmware V1.6.1)

## Building

Requires a stable Rust toolchain (edition 2024).

```sh
cargo build --workspace
```

## Usage

```sh
# Identity and status
kcsdi info --host 192.0.2.10 --port 901

# Spectrum sweep to CSV
kcsdi sweep spec --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 --rbw 10k \
    --out spec.csv

# S11 sweep to CSV
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 1000000 --stop 100000000 --points 201 --format loss \
    --out s11.csv
```

## Protocol References

The protocol layer is informed by the KC901 programming manual
(B002-008, 3rd edition, KeXinShe) and cross-checked against the
documented device behavior of the KCSDI application (MEASALL Technology).
See `the protocol reference` for the full reference and NOTICE
for credits.

## License

MIT. See LICENSE and NOTICE.
