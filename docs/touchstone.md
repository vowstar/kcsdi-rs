<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# Touchstone export and verification

The exporter implements the single-ended S-parameter subset of
[Touchstone 1.0/2.0](https://ibis.org/touchstone_ver2.0/touchstone_ver2_0.pdf).
Both syntax versions use `# Hz S RI R 50` and the `.s1p` or `.s2p`
extension. Version 2.0 explicitly declares ports, frequency count, full
matrix, data order and end marker. Version 1.0 omits bracketed keywords
for older readers. This is an exporter, not a general Touchstone importer.

## Input data and boundaries

One-port export accepts complex S11 in these CSV representations:

| CSV header | Interpretation |
| --- | --- |
| `freq_hz,real,imag` | Dimensionless S-parameter real and imaginary parts |
| `freq_hz,magnitude,phase_deg` | Linear magnitude and phase in degrees |
| `freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm` | Reflection impedance, converted using `(Z - 50)/(Z + 50)` |

The impedance magnitude column is validated but R and X determine the
conversion. Scalar return loss, VSWR, delay, spectrum levels, and ambiguous
column headers are rejected. They do not contain a complete complex
S-parameter. Export does not perform calibration, de-embedding or
renormalization. Complex RI/MA inputs must already use a 50 ohm reference.

Two-port export requires S11, S21, S12 and S22. Each CSV is identified by
its CLI argument, not its filename. Reflection inputs may use RI, MA or
Z. Transmission inputs must use RI or MA. The legacy serialization order
is S11, S21, S12, S22. Version 2.0 also declares
`[Two-Port Data Order] 21_12` explicitly.

All four measurements must describe the same device, port numbering,
reference planes, reference impedance and measurement conditions. Those
conditions cannot be established from numeric CSVs. The operator must
ensure compatibility. The assembler checks exact frequency equality and
point count. It does not interpolate, extrapolate, fill missing terms with
zero or assume reciprocity. The application does not yet implement a
complete two-port acquisition workflow.

Every input needs at least one sample. Frequencies must be finite,
non-negative and strictly increasing. A supplied DC sample is retained,
but export support for DC does not imply the instrument can measure S11
at DC. Values must be finite, with non-negative magnitudes and a valid
impedance-to-S conversion. Values outside the passive unit circle are
preserved. Frequencies are the actual reported values, including firmware
rounding, not reconstructed from the requested sweep endpoints.

The GUI freezes the last completed S11 sweep before opening the native
save dialog. It can export after disconnecting. The current display tab,
axis scaling, zoom, hidden curves and later sweeps do not alter that
snapshot. A scalar trace requires another scan in Phase, Smith or
Impedance. Merely switching tabs does not add missing phase.

Output is validated before opening a destination and saved using a
temporary file in the destination directory. Existing Touchstone files
are protected unless replacement is explicitly permitted. A wrong
extension is rejected. The GUI appends `.s1p` only if there is no extension.

## Independent verification

Normal Rust tests require no external tools:

```sh
cargo check --workspace
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --workspace
```

For independent validation, obtain the official
[IBIS tschk2 2.0.0 binary](https://ibis.org/tschk2/v200/tschk2.htm) for your
platform and follow its included usage terms. Install scikit-rf into a
separate Python environment. The project does not download or package
the checker. For example, with paths adjusted for your machine:

```sh
python3 -m venv /tmp/kcsdi-touchstone-venv
/tmp/kcsdi-touchstone-venv/bin/pip install scikit-rf==2.1.0
/tmp/kcsdi-touchstone-venv/bin/python scripts/verify_touchstone.py \
    --kcsdi target/debug/kcsdi --tschk2 /path/to/tschk2
```

The script generates eight valid exports covering both versions and port
counts, using synthetic asymmetric, non-reciprocal data, zero frequency,
fractional Hz, RI/MA/Z conversion and active values. Each file must pass
tschk2 without warnings. It then uses the independent
[scikit-rf Network reader](https://scikit-rf.readthedocs.io/en/latest/tutorials/Networks.html)
to compare all frequencies, complex matrix entries and reference
impedances. Negative cases check invalid data, mismatched grids and
protection of existing files, including when `--overwrite` is present.

Add `--s11-csv /path/to/complex.csv` to check recorded S11 data in both
versions. The option can be repeated. Tests never contact the instrument.
On 2026-09-07, tschk2 2.0.0 and scikit-rf 2.1.0 passed all eight synthetic
exports and eight exports from four recorded KC901V S11 sweeps, including
a full-range scan and RI, MA and Z packets. File validity and numerical
round-trip checks do not establish instrument accuracy or calibration
quality. Native save dialogs still need desktop testing on each platform.
