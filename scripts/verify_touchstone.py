# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

"""Independent CLI export checks. Requires numpy, scikit-rf and IBIS tschk2.

Uses generated fixtures, never connects to an instrument. Optional --s11-csv
inputs must use the documented complex CSV headers. All outputs are temporary.
"""

import argparse
import csv
import subprocess
import tempfile
from pathlib import Path

import numpy as np
import skrf


def run(arguments, success=True):
    result = subprocess.run(arguments, capture_output=True, text=True, timeout=30)
    if (result.returncode == 0) != success:
        raise AssertionError(f"Unexpected exit {result.returncode}: {arguments}\n"
                             f"{result.stdout}{result.stderr}")
    return result


def write_csv(path, header, rows):
    with path.open("w", newline="", encoding="ascii") as output:
        writer = csv.writer(output)
        writer.writerow(header.split(","))
        writer.writerows(rows)


def expected_csv(path):
    with path.open(newline="", encoding="ascii") as source:
        reader = csv.reader(source)
        header = next(reader)
        values = np.asarray(list(reader), dtype=float)
    frequency = values[:, 0]
    if header == ["freq_hz", "real", "imag"]:
        expected = values[:, 1] + 1j * values[:, 2]
    elif header == ["freq_hz", "magnitude", "phase_deg"]:
        expected = values[:, 1] * np.exp(1j * np.deg2rad(values[:, 2]))
    elif header == ["freq_hz", "z_mag_ohm", "resistance_ohm", "reactance_ohm"]:
        impedance = values[:, 2] + 1j * values[:, 3]
        expected = (impedance - 50) / (impedance + 50)
    else:
        raise ValueError(f"Unsupported CSV header: {path}")
    return frequency, expected[:, None, None]


def verify(path, frequency, expected, checker):
    result = run([checker, str(path)])
    # Require only the official success line, with no warnings.
    version = "2.0" if "[Version] 2.0" in path.read_text(encoding="ascii") else "1"
    diagnostics = (result.stdout + result.stderr).strip()
    if diagnostics != f"{path}: valid Touchstone v{version} file":
        raise AssertionError(f"Checker diagnostics for {path}: {result.stdout}{result.stderr}")
    network = skrf.Network(str(path))
    np.testing.assert_array_equal(network.f, frequency)
    np.testing.assert_allclose(network.s, expected, rtol=2e-14, atol=2e-15)
    np.testing.assert_array_equal(network.z0, np.full(expected.shape[:2], 50.0))
    assert network.nports == expected.shape[1]


def check_exports(directory, cli, checker, extra_csvs):
    frequency = np.array([0.0, 5000.0, 1234567.8901234567, 7000000200.0])
    # Non-reciprocal, asymmetric and active values expose port transposition
    # and accidental passivity clamping. These are synthetic, not hardware data.
    parameters = {
        "s11": [0.25 - 0.5j, 0j, -1j, 1.2 + 0.1j],
        "s21": [0.81 + 0.2j, 2 - 0.1j, -0.3 + 0.12j, 0.41 - 0.22j],
        "s12": [-0.11 + 0.32j, 0.14 + 0.01j, 0.05 - 0.06j, -0.22 - 0.13j],
        "s22": [0.01 + 0.09j, -0.2 + 0.4j, 0.73 + 0.01j, 0.17 - 0.19j],
    }
    paths = {}
    expected = np.empty((len(frequency), 2, 2), dtype=complex)
    for parameter, values in parameters.items():
        path = directory / f"{parameter}.csv"
        paths[parameter] = path
        write_csv(path, "freq_hz,real,imag",
                  ((f, v.real, v.imag) for f, v in zip(frequency, values)))
        expected[:, int(parameter[1]) - 1, int(parameter[2]) - 1] = values
    ma = directory / "ma.csv"
    write_csv(ma, "freq_hz,magnitude,phase_deg",
              zip(frequency, [2, 0, 0.131, 1], [90, -180, -12.708, 765]))
    impedance = directory / "z.csv"
    write_csv(impedance, "freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm",
              [(f, abs(z), z.real, z.imag)
               for f, z in zip(frequency, [50 + 0j, 0j, 50j, -25 + 0j])])
    count = 0
    for version in ["1", "2"]:
        for index, source in enumerate([paths["s11"], ma, impedance, *extra_csvs]):
            output = directory / f"one-{version}-{index}.s1p"
            run([cli, "export", "s1p", "--input", str(source), "--out", str(output),
                 "--touchstone-version", version])
            verify(output, *expected_csv(source), checker)
            original = output.read_bytes()
            run([cli, "export", "s1p", "--input", str(source), "--out", str(output)], success=False)
            assert output.read_bytes() == original
            count += 1
        output = directory / f"two-{version}.s2p"
        arguments = [cli, "export", "s2p", "--out", str(output), "--touchstone-version", version]
        for parameter, path in paths.items():
            arguments.extend([f"--{parameter}", str(path)])
        run(arguments)
        verify(output, frequency, expected, checker)
        count += 1
        # Even explicit overwrite cannot bypass mismatched frequency rejection.
        original = output.read_bytes()
        bad = directory / "wrong-grid.csv"
        write_csv(bad, "freq_hz,real,imag", [(1, 0.1, 0.2)])
        arguments[arguments.index("--s12") + 1] = str(bad)
        run([*arguments, "--overwrite"], success=False)
        assert output.read_bytes() == original

    invalid_csvs = [
        ("freq_hz,loss_db", [(5000, 10)]),
        ("freq_hz,real,imag", [(5000, "NaN", 0)]),
        ("freq_hz,real,imag", [(5000, 0, "inf")]),
        ("freq_hz,real,imag", [(5000, 0, 0), (5000, 1, 0)]),
        ("freq_hz,real,imag", [(6000, 0, 0), (5000, 1, 0)]),
        ("freq_hz,real,imag", [(-1, 0, 0)]),
        ("freq_hz,magnitude,phase_deg", [(5000, -1, 90)]),
        ("freq_hz,z_mag_ohm,resistance_ohm,reactance_ohm", [(5000, 50, -50, 0)]),
        ("freq_hz,real,imag", []),
    ]
    output = directory / "protected.s1p"
    output.write_bytes(b"existing file\n")
    for header, rows in invalid_csvs:
        source = directory / "invalid.csv"
        write_csv(source, header, rows)
        run([cli, "export", "s1p", "--input", str(source), "--out", str(output),
             "--overwrite"], success=False)
        assert output.read_bytes() == b"existing file\n"
    # A deliberately malformed file confirms that the checker is actually run.
    invalid = directory / "invalid.s1p"
    invalid.write_text("[Version] 2.0\n# Hz S RI R 50\n[End]\n", encoding="ascii")
    run([checker, str(invalid)], success=False)
    return count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kcsdi", type=Path, required=True)
    parser.add_argument("--tschk2", type=Path, required=True)
    parser.add_argument("--s11-csv", type=Path, action="append", default=[])
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="kcsdi-touchstone-test-") as directory:
        count = check_exports(Path(directory), str(args.kcsdi.resolve()),
                              str(args.tschk2.resolve()), args.s11_csv)
    print(f"PASS: {count} exports checked by tschk2 and scikit-rf {skrf.__version__}")
    print("PASS: invalid data, grid mismatch and overwrite protection")


if __name__ == "__main__":
    main()
