<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# Touchstone export

The GUI and CLI save `.s1p` files. The CLI also assembles `.s2p` files from four complex measurements. Both [Touchstone versions](https://ibis.org/touchstone_ver2.0/touchstone_ver2_0.pdf) use Hz, real and imaginary values, and a 50 ohm reference.

The default is version 2.0. For version 1, select it in the GUI or use `--touchstone-version 1`. To replace an existing file, use `--overwrite` in the CLI or accept the GUI confirmation.

## CSV inputs

The column header determines the input format:

| Header | Values | Use |
| --- | --- | --- |
| `freq_hz,real,imag` | Complex S-parameter | Reflection or transmission |
| `freq_hz,magnitude,phase_deg` | Linear magnitude and phase in degrees | Reflection or transmission |
| `freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm` | Impedance in ohms | Reflection |

All complex S-parameters must use a 50 ohm reference. Impedance conversion uses `S = (Z - 50) / (Z + 50)`, with `Z = R + jX`. The magnitude column must be valid, but R and X determine the conversion.

Every file needs at least one sample. Frequencies must be finite, non-negative and strictly increasing. All values must be finite, and magnitudes must be non-negative. Scalar return loss, VSWR and spectrum data cannot supply the missing phase.

For `.s2p`, supply S11, S21, S12 and S22 from the same device, frequency grid, port definitions, reference planes and measurement conditions. The assembler requires equal point counts and exact frequency matches. The user must establish the other conditions. The file order is S11, S21, S12, S22.

Export preserves actual frequencies, including firmware rounding and supplied DC samples. It also preserves values outside the passive unit circle. Export does not change calibration or reference planes.

## GUI export

Run a sweep in Phase, Smith or Impedance before export. Switching tabs after a scalar sweep does not supply phase. The GUI saves the last completed complex sweep in full, even after disconnection.

Zoom, hidden traces and subsequent sweeps leave the export snapshot unchanged. The GUI adds `.s1p` if the filename has no extension. It rejects other extensions. The exporter checks the data before it opens the destination and replaces files through a temporary file.

## Independent validation

The test script compares exported data with the [scikit-rf reader](https://scikit-rf.readthedocs.io/en/latest/tutorials/Networks.html). It also requires a clean result from the official [IBIS tschk2 2.0.0 checker](https://ibis.org/tschk2/v200/tschk2.htm). Download the checker for your platform from IBIS.

```sh
cargo build --workspace --locked
python3 -m venv /tmp/kcsdi-touchstone-venv
/tmp/kcsdi-touchstone-venv/bin/pip install scikit-rf==2.1.0
/tmp/kcsdi-touchstone-venv/bin/python scripts/verify_touchstone.py \
    --kcsdi target/debug/kcsdi --tschk2 /path/to/tschk2
```

The script covers both versions, both port counts, RI/MA/Z conversion, asymmetric matrices and invalid inputs. It compares frequencies, complex values, port order and reference impedance. It also makes sure that failed exports preserve existing files.

To include recorded S11 data, add `--s11-csv /path/to/complex.csv`. This option accepts multiple occurrences. The script uses temporary files and does not connect to an instrument.
