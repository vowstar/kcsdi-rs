// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Numeric exports of immutable, complete measurements and their own settings.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use kcsdi_core::device::PointSettings;
use kcsdi_core::table::{self, Column};
use rust_xlsxwriter::{Workbook, Worksheet, XlsxError};

use crate::acquisition::{AcquisitionSettings, CompletedSweep, MAX_TRACES, TraceId};

const VALUE_HEADERS: [&str; 6] = [
    "trace_id",
    "point_index",
    "freq_hz",
    "quantity",
    "value",
    "unit",
];
const METADATA_HEADERS: [&str; 13] = [
    "source",
    "mode",
    "format",
    "calibration",
    "rbw",
    "lo",
    "reference_dbm",
    "requested_start_hz",
    "requested_stop_hz",
    "requested_points",
    "completed_at_unix_s",
    "session_id",
    "frequency_plan",
];
const REQUESTED_FREQUENCY_HEADER: &str = "requested_freq_hz";
const SEGMENT_HEADERS: [&str; 8] = [
    "segment_index",
    "segment_point_index",
    "segment_start_hz",
    "segment_stop_hz",
    "segment_max_step_hz",
    "segment_requested_points",
    "segment_received_points",
    "segment_retained_points",
];

/// A bounded selection frozen before a file dialog or background serialization.
#[derive(Debug, Clone)]
pub struct FrozenSnapshots {
    traces: Vec<(TraceId, Arc<CompletedSweep>)>,
}

impl FrozenSnapshots {
    pub fn new(traces: Vec<(TraceId, Arc<CompletedSweep>)>) -> Result<Self, String> {
        if traces.is_empty() || traces.len() > MAX_TRACES {
            return Err(format!(
                "export requires between 1 and {MAX_TRACES} completed traces"
            ));
        }
        let mut ids = BTreeSet::new();
        for (id, snapshot) in &traces {
            if id.0 == 0 || !ids.insert(id.0) {
                return Err(format!("invalid or repeated export trace ID {}", id.0));
            }
            validate(snapshot).map_err(|error| format!("T{}: {error}", id.0))?;
        }
        Ok(Self { traces })
    }

    pub fn len(&self) -> usize {
        self.traces.len()
    }

    pub fn point_count(&self) -> usize {
        self.traces
            .iter()
            .map(|(_, snapshot)| snapshot.data.points.len())
            .sum()
    }

    /// One rectangular record per raw value. Point indices preserve repeated Hz.
    pub fn csv_bytes(&self) -> Result<Vec<u8>, String> {
        let segmented = self
            .traces
            .iter()
            .any(|(_, snapshot)| snapshot.segments.is_some());
        let mut writer = csv::WriterBuilder::new()
            .terminator(csv::Terminator::CRLF)
            .from_writer(Vec::new());
        writer
            .write_record(
                VALUE_HEADERS
                    .into_iter()
                    .chain(METADATA_HEADERS)
                    .chain([REQUESTED_FREQUENCY_HEADER])
                    .chain(SEGMENT_HEADERS.into_iter().take(if segmented {
                        SEGMENT_HEADERS.len()
                    } else {
                        0
                    })),
            )
            .map_err(|error| error.to_string())?;
        for (id, snapshot) in &self.traces {
            let metadata = metadata(snapshot);
            let segments = segment_rows(snapshot);
            let columns = schema(snapshot)?;
            for (index, point) in snapshot.data.points.iter().enumerate() {
                let requested = requested_frequencies(&snapshot.settings)
                    .map_or_else(String::new, |frequencies| frequencies[index].to_string());
                for (column, &value) in columns.iter().zip(&point.values) {
                    let mut record = vec![
                        id.0.to_string(),
                        index.to_string(),
                        point.freq_hz.to_string(),
                        column.name.to_owned(),
                        numeric_text(value),
                        column.unit.to_owned(),
                    ];
                    record.extend(metadata.iter().cloned());
                    record.push(requested.clone());
                    if segmented {
                        if let Some(origin) = snapshot
                            .segments
                            .as_ref()
                            .map(|metadata| metadata.origins[index])
                        {
                            record.push(origin.segment.to_string());
                            record.push(origin.point.to_string());
                            record.extend(segments[origin.segment].iter().cloned());
                        } else {
                            record
                                .extend(std::iter::repeat_n(String::new(), SEGMENT_HEADERS.len()));
                        }
                    }
                    writer
                        .write_record(&record)
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        writer.into_inner().map_err(|error| error.to_string())
    }

    /// Keep each trace's original grid in its own worksheet.
    pub fn xlsx_bytes(&self) -> Result<Vec<u8>, String> {
        self.workbook()
            .map_err(|error| error.to_string())?
            .save_to_buffer()
            .map_err(|error| error.to_string())
    }

    fn workbook(&self) -> Result<Workbook, XlsxError> {
        let mut workbook = Workbook::new();
        for (id, snapshot) in &self.traces {
            // Construction already validated the schema and row width.
            let columns = table::columns(snapshot.data.mode, &snapshot.data.format)
                .expect("validated frozen raw-column schema");
            let worksheet = workbook.add_worksheet();
            worksheet.set_name(format!("T{}", id.0))?;
            worksheet.write_string(0, 0, "freq_hz")?;
            for (index, column) in columns.iter().enumerate() {
                worksheet.write_string(0, index as u16 + 1, column.name)?;
            }
            let requested = requested_frequencies(&snapshot.settings);
            let requested_column = columns.len() as u16 + 1;
            if requested.is_some() {
                worksheet.write_string(0, requested_column, REQUESTED_FREQUENCY_HEADER)?;
            }
            if snapshot.segments.is_some() {
                worksheet.write_string(0, requested_column, SEGMENT_HEADERS[0])?;
                worksheet.write_string(0, requested_column + 1, SEGMENT_HEADERS[1])?;
            }
            worksheet.set_column_range_width(
                0,
                columns.len() as u16 + u16::from(requested.is_some()),
                24,
            )?;
            worksheet.set_freeze_panes(1, 1)?;
            for (index, point) in snapshot.data.points.iter().enumerate() {
                let row = index as u32 + 1;
                worksheet.write_number(row, 0, point.freq_hz)?;
                for (index, &value) in point.values.iter().enumerate() {
                    write_value(worksheet, row, index as u16 + 1, value)?;
                }
                if let Some(frequencies) = requested {
                    worksheet.write_number(row, requested_column, frequencies[index] as f64)?;
                }
                if let Some(metadata) = &snapshot.segments {
                    let origin = metadata.origins[index];
                    worksheet.write_number(row, requested_column, origin.segment as f64)?;
                    worksheet.write_number(row, requested_column + 1, origin.point as f64)?;
                }
            }
        }
        let worksheet = workbook.add_worksheet();
        worksheet.set_name("Metadata")?;
        worksheet.set_column_range_width(0, METADATA_HEADERS.len() as u16, 24)?;
        worksheet.set_freeze_panes(1, 1)?;
        for (index, header) in std::iter::once("trace_id")
            .chain(METADATA_HEADERS)
            .enumerate()
        {
            worksheet.write_string(0, index as u16, header)?;
        }
        for (index, (id, snapshot)) in self.traces.iter().enumerate() {
            let row = index as u32 + 1;
            // IDs can exceed the exact integer range of spreadsheet numbers.
            worksheet.write_string(row, 0, id.0.to_string())?;
            for (index, value) in metadata(snapshot).iter().enumerate() {
                worksheet.write_string(row, index as u16 + 1, value)?;
            }
        }
        if self
            .traces
            .iter()
            .any(|(_, snapshot)| snapshot.segments.is_some())
        {
            let sheet = workbook.add_worksheet();
            sheet.set_name("Segments")?;
            sheet.set_freeze_panes(1, 2)?;
            sheet.set_column_range_width(0, 7, 24)?;
            for (column, header) in ["trace_id", SEGMENT_HEADERS[0]]
                .into_iter()
                .chain(SEGMENT_HEADERS[2..].iter().copied())
                .enumerate()
            {
                sheet.write_string(0, column as u16, header)?;
            }
            let mut row = 1;
            for (id, snapshot) in &self.traces {
                for (index, values) in segment_rows(snapshot).into_iter().enumerate() {
                    sheet.write_string(row, 0, id.0.to_string())?;
                    sheet.write_number(row, 1, index as f64)?;
                    for (column, value) in values.iter().enumerate() {
                        sheet.write_string(row, column as u16 + 2, value)?;
                    }
                    row += 1;
                }
            }
        }
        Ok(workbook)
    }
}

fn validate(snapshot: &CompletedSweep) -> Result<(), String> {
    if snapshot.session_id == 0 {
        return Err("completed sweep has no session identity".into());
    }
    snapshot
        .settings
        .validate()
        .map_err(|error| error.to_string())?;
    if !snapshot.accepts_data() {
        return Err(
            "completed data does not match its captured mode, format or point count".into(),
        );
    }
    let columns = schema(snapshot)?;
    let finite_range = requested_frequencies(&snapshot.settings).is_none();
    let mut previous = None;
    for (index, point) in snapshot.data.points.iter().enumerate() {
        if !point.freq_hz.is_finite()
            || point.freq_hz < 0.0
            || (finite_range && previous.is_some_and(|frequency| point.freq_hz < frequency))
        {
            return Err(format!("sample {index} has an invalid measured frequency"));
        }
        if point.values.len() != columns.len() {
            return Err(format!("sample {index} has the wrong number of raw values"));
        }
        previous = Some(point.freq_hz);
    }
    Ok(())
}

fn requested_frequencies(settings: &AcquisitionSettings) -> Option<&[u64]> {
    match settings {
        AcquisitionSettings::List { frequencies_hz, .. } => Some(frequencies_hz),
        _ => None,
    }
}

fn schema(snapshot: &CompletedSweep) -> Result<&'static [Column], String> {
    table::columns(snapshot.data.mode, &snapshot.data.format)
        .ok_or_else(|| "unsupported raw measurement format".into())
}

fn metadata(snapshot: &CompletedSweep) -> [String; METADATA_HEADERS.len()] {
    let (cal, rbw, lo, reference) = match snapshot.settings.receiver() {
        PointSettings::S11 { cal, rbw, .. } => (cal, rbw, None, None),
        PointSettings::S21 { cal, rbw, lo, .. } => (cal, rbw, Some(lo), None),
        PointSettings::Spec {
            cal,
            rbw,
            lo,
            ref_level_dbm,
        } => (cal, Some(rbw), Some(lo), Some(ref_level_dbm)),
    };
    let (start, stop, kind) = match &snapshot.settings {
        AcquisitionSettings::S11(params) => (params.start_hz, params.stop_hz, "range"),
        AcquisitionSettings::S21(params) => (params.start_hz, params.stop_hz, "range"),
        AcquisitionSettings::Spec(params) => (params.start_hz, params.stop_hz, "range"),
        AcquisitionSettings::List { frequencies_hz, .. } => (
            frequencies_hz[0],
            *frequencies_hz.last().expect("validated frequency list"),
            "list",
        ),
        AcquisitionSettings::Segments(plan) => (
            plan.segments()[0].definition().start_hz,
            plan.segments()
                .last()
                .expect("validated segment plan")
                .definition()
                .stop_hz,
            "segments",
        ),
    };
    [
        "measured".into(),
        snapshot.data.mode.name().into(),
        snapshot.data.format.clone(),
        cal.as_str().into(),
        rbw.map_or("", |rbw| rbw.as_str()).into(),
        lo.map_or("", |lo| lo.as_str()).into(),
        reference.map_or_else(String::new, |value| value.to_string()),
        start.to_string(),
        stop.to_string(),
        snapshot.settings.points().to_string(),
        unix_timestamp(snapshot.completed_at),
        snapshot.session_id.to_string(),
        kind.into(),
    ]
}

fn segment_rows(snapshot: &CompletedSweep) -> Vec<[String; 6]> {
    let (AcquisitionSettings::Segments(plan), Some(metadata)) =
        (&snapshot.settings, &snapshot.segments)
    else {
        return Vec::new();
    };
    plan.segments()
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let definition = segment.definition();
            [
                definition.start_hz.to_string(),
                definition.stop_hz.to_string(),
                definition.max_step_hz.to_string(),
                segment.points().to_string(),
                metadata.received[index].to_string(),
                metadata.retained(index).to_string(),
            ]
        })
        .collect()
}

fn unix_timestamp(time: SystemTime) -> String {
    let (sign, duration) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => ("", duration),
        Err(error) => ("-", error.duration()),
    };
    format!(
        "{sign}{}.{:09}",
        duration.as_secs(),
        duration.subsec_nanos()
    )
}

fn numeric_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value == f64::INFINITY {
        "+Inf".into()
    } else if value == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        value.to_string()
    }
}

fn write_value(
    worksheet: &mut Worksheet,
    row: u32,
    column: u16,
    value: f64,
) -> Result<(), XlsxError> {
    if value.is_finite() {
        worksheet.write_number(row, column, value)?;
    } else {
        worksheet.write_string(row, column, numeric_text(value))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    use calamine::{Data, Reader, Xlsx, open_workbook_from_rs};
    use kcsdi_core::commands::{Cal, Format, Lo};
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::model::Rbw;
    use kcsdi_core::protocol::StreamMode;

    fn snapshot(mode: StreamMode, format: &str, count: u32) -> CompletedSweep {
        let settings = match mode {
            StreamMode::S11 => AcquisitionSettings::S11(kcsdi_core::device::S11Params {
                cal: Cal::CalUser,
                format: format.parse().unwrap(),
                points: count,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Some(Rbw::R3k),
            }),
            StreamMode::S21 => AcquisitionSettings::S21(kcsdi_core::device::S21Params {
                cal: Cal::CalSys,
                format: format.parse().unwrap(),
                lo: Lo::LowLo,
                points: count,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Some(Rbw::R3k),
            }),
            StreamMode::Spec => AcquisitionSettings::Spec(kcsdi_core::device::SpecParams {
                cal: Cal::CalOff,
                lo: Lo::HighLo,
                points: count,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Rbw::R3k,
                ref_level_dbm: -20,
            }),
            _ => unreachable!(),
        };
        let row = match format {
            "ri" => vec![0.125, -0.25],
            "ma" => vec![0.75, -45.125],
            "vswr" => vec![1.2345],
            "z" => vec![75.25, 50.0, -25.25],
            "loss" => vec![-25.25],
            "delay" => vec![-4.25678912345e-12],
            "" => vec![-72.25],
            _ => unreachable!(),
        };
        let points = (0..count)
            .map(|index| SweepPoint {
                freq_hz: 1_000_000.125 + f64::from(index) * 1_000_000.125 / f64::from(count - 1),
                values: row
                    .iter()
                    .map(|value| value * f64::from(index + 1))
                    .collect(),
            })
            .collect();
        CompletedSweep {
            segments: None,
            data: SweepData {
                mode,
                format: format.into(),
                points,
            },
            settings,
            session_id: u64::MAX - 9,
            // Windows SystemTime represents whole 100 ns ticks.
            completed_at: UNIX_EPOCH + Duration::new(1_789_021_234, 987_654_300),
        }
    }

    fn workbook(bytes: Vec<u8>) -> Xlsx<Cursor<Vec<u8>>> {
        open_workbook_from_rs(Cursor::new(bytes)).unwrap()
    }

    fn list_snapshot(mode: StreamMode, format: &str, frequencies_hz: Vec<u64>) -> CompletedSweep {
        let mut snapshot = snapshot(mode, format, frequencies_hz.len() as u32);
        let settings = match snapshot.settings {
            AcquisitionSettings::S11(params) => PointSettings::S11 {
                cal: params.cal,
                format: params.format,
                rbw: params.rbw,
            },
            AcquisitionSettings::S21(params) => PointSettings::S21 {
                cal: params.cal,
                format: params.format,
                lo: params.lo,
                rbw: params.rbw,
            },
            AcquisitionSettings::Spec(params) => PointSettings::Spec {
                cal: params.cal,
                lo: params.lo,
                rbw: params.rbw,
                ref_level_dbm: params.ref_level_dbm,
            },
            AcquisitionSettings::List { .. } | AcquisitionSettings::Segments(_) => unreachable!(),
        };
        snapshot.settings = AcquisitionSettings::List {
            settings,
            frequencies_hz,
        };
        snapshot
    }

    fn assert_no_formulas(workbook: &mut Xlsx<Cursor<Vec<u8>>>) {
        for name in workbook.sheet_names().to_vec() {
            assert!(
                workbook
                    .worksheet_formula(&name)
                    .unwrap()
                    .used_cells()
                    .all(|(_, _, formula)| formula.is_empty()),
                "formula in {name}"
            );
        }
    }

    #[test]
    fn csv_and_xlsx_readers_recover_all_supported_raw_formats_and_metadata() {
        let cases = [
            (StreamMode::S11, "ri", vec![("real", ""), ("imag", "")]),
            (
                StreamMode::S11,
                "ma",
                vec![("magnitude", ""), ("phase_deg", "deg")],
            ),
            (StreamMode::S11, "vswr", vec![("vswr", "")]),
            (
                StreamMode::S11,
                "z",
                vec![
                    ("z_mag_ohm", "ohm"),
                    ("resistance_ohm", "ohm"),
                    ("reactance_ohm", "ohm"),
                ],
            ),
            (StreamMode::S11, "loss", vec![("loss_db", "dB")]),
            (StreamMode::S21, "ri", vec![("real", ""), ("imag", "")]),
            (
                StreamMode::S21,
                "ma",
                vec![("magnitude", ""), ("phase_deg", "deg")],
            ),
            (StreamMode::S21, "loss", vec![("loss_db", "dB")]),
            (StreamMode::S21, "delay", vec![("delay_s", "s")]),
            (StreamMode::Spec, "", vec![("level_dbm", "dBm")]),
        ];
        let traces: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (mode, format, _))| {
                let mut snapshot = snapshot(*mode, format, 3);
                snapshot.data.points[1].freq_hz = snapshot.data.points[0].freq_hz;
                // Each trace owns a distinct actual grid and completion timestamp.
                for point in &mut snapshot.data.points {
                    point.freq_hz += index as f64 / 16.0;
                }
                snapshot.completed_at += Duration::from_nanos(index as u64 * 100);
                (
                    TraceId(9_007_199_254_740_993 + index as u64),
                    Arc::new(snapshot),
                )
            })
            .collect();
        let frozen = FrozenSnapshots::new(traces.clone()).unwrap();
        assert_eq!(frozen.len(), 10);
        assert_eq!(frozen.point_count(), 30);
        let csv = frozen.csv_bytes().unwrap();
        let mut reader = csv::Reader::from_reader(csv.as_slice());
        let headers = reader.headers().unwrap().clone();
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            VALUE_HEADERS
                .into_iter()
                .chain(METADATA_HEADERS)
                .chain([REQUESTED_FREQUENCY_HEADER])
                .collect::<Vec<_>>()
        );
        let mut records = reader.records();
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        assert_eq!(xlsx.sheet_names().len(), 11);
        let metadata = xlsx.worksheet_range("Metadata").unwrap();
        for (trace_index, ((id, snapshot), (_, _, columns))) in
            traces.iter().zip(&cases).enumerate()
        {
            let data = xlsx.worksheet_range(&format!("T{}", id.0)).unwrap();
            assert_eq!(data.get_size(), (4, columns.len() + 1));
            assert_eq!(
                data.get_value((0, 0)),
                Some(&Data::String("freq_hz".into()))
            );
            for (column_index, (name, _)) in columns.iter().enumerate() {
                assert_eq!(
                    data.get_value((0, column_index as u32 + 1)),
                    Some(&Data::String((*name).into()))
                );
            }
            for (point_index, point) in snapshot.data.points.iter().enumerate() {
                assert_eq!(
                    data.get_value((point_index as u32 + 1, 0)),
                    Some(&Data::Float(point.freq_hz))
                );
                for (column_index, ((quantity, unit), value)) in
                    columns.iter().zip(&point.values).enumerate()
                {
                    let record = records.next().unwrap().unwrap();
                    assert_eq!(record.len(), 20);
                    assert_eq!(&record[0], id.0.to_string());
                    assert_eq!(&record[1], point_index.to_string());
                    assert_eq!(record[2].parse::<f64>().unwrap(), point.freq_hz);
                    assert_eq!(&record[3], *quantity);
                    assert_eq!(record[4].parse::<f64>().unwrap(), *value);
                    assert_eq!(&record[5], *unit);
                    assert_eq!(&record[6], "measured");
                    assert_eq!(&record[7], snapshot.data.mode.name());
                    assert_eq!(&record[8], snapshot.data.format);
                    assert_eq!(&record[10], "3k");
                    assert_eq!(&record[13], "1000000");
                    assert_eq!(&record[14], "2000000");
                    assert_eq!(&record[15], "3");
                    assert_eq!(
                        &record[16],
                        format!("1789021234.{:09}", 987_654_300 + trace_index * 100)
                    );
                    assert_eq!(&record[17], (u64::MAX - 9).to_string());
                    assert_eq!(&record[18], "range");
                    assert_eq!(&record[19], "");
                    match snapshot.data.mode {
                        StreamMode::S11 => {
                            assert_eq!((&record[9], &record[11], &record[12]), ("caluser", "", ""))
                        }
                        StreamMode::S21 => assert_eq!(
                            (&record[9], &record[11], &record[12]),
                            ("calsys", "lowlo", "")
                        ),
                        StreamMode::Spec => assert_eq!(
                            (&record[9], &record[11], &record[12]),
                            ("caloff", "highlo", "-20")
                        ),
                        _ => unreachable!(),
                    }
                    assert_eq!(
                        data.get_value((point_index as u32 + 1, column_index as u32 + 1)),
                        Some(&Data::Float(*value))
                    );
                }
            }
            let row = trace_index as u32 + 1;
            assert_eq!(
                metadata.get_value((row, 0)),
                Some(&Data::String(id.0.to_string()))
            );
            assert_eq!(
                metadata.get_value((row, 1)),
                Some(&Data::String("measured".into()))
            );
            assert_eq!(
                metadata.get_value((row, 2)),
                Some(&Data::String(snapshot.data.mode.name().into()))
            );
            assert_eq!(
                metadata.get_value((row, 11)),
                Some(&Data::String(format!(
                    "1789021234.{:09}",
                    987_654_300 + trace_index * 100
                )))
            );
            assert_eq!(
                metadata.get_value((row, 12)),
                Some(&Data::String((u64::MAX - 9).to_string()))
            );
            assert_eq!(
                metadata.get_value((row, 13)),
                Some(&Data::String("range".into()))
            );
        }
        assert!(records.next().is_none());
        assert_no_formulas(&mut xlsx);
    }

    #[test]
    fn nonfinite_measurements_are_literal_text_and_finite_values_stay_numeric() {
        let mut snapshot = snapshot(StreamMode::S11, "z", 3);
        snapshot.data.points[0].values = vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        snapshot.data.points[1].values = vec![f64::MIN_POSITIVE, f64::MAX, -0.0];
        let frozen = FrozenSnapshots::new(vec![(TraceId(1), Arc::new(snapshot))]).unwrap();
        let csv = frozen.csv_bytes().unwrap();
        let records: Vec<_> = csv::Reader::from_reader(csv.as_slice())
            .records()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            [&records[0][4], &records[1][4], &records[2][4]],
            ["NaN", "+Inf", "-Inf"]
        );
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        let data = xlsx.worksheet_range("T1").unwrap();
        for (index, expected) in ["NaN", "+Inf", "-Inf"].into_iter().enumerate() {
            assert_eq!(
                data.get_value((1, index as u32 + 1)),
                Some(&Data::String(expected.into()))
            );
        }
        for (index, expected) in [f64::MIN_POSITIVE, f64::MAX, -0.0].into_iter().enumerate() {
            assert_eq!(
                data.get_value((2, index as u32 + 1)),
                Some(&Data::Float(expected))
            );
        }
        assert_no_formulas(&mut xlsx);
    }

    #[test]
    fn list_exports_keep_requested_indices_and_actual_jitter_in_every_mode() {
        let requested = [1_000_000, 1_000_000, 1_300_007];
        let actual = [1_000_000.25, 999_999.875, 1_300_007.5];
        let cases = [
            (StreamMode::S11, "ri"),
            (StreamMode::S11, "ma"),
            (StreamMode::S11, "vswr"),
            (StreamMode::S11, "z"),
            (StreamMode::S11, "loss"),
            (StreamMode::S21, "ri"),
            (StreamMode::S21, "ma"),
            (StreamMode::S21, "loss"),
            (StreamMode::S21, "delay"),
            (StreamMode::Spec, ""),
        ];
        for (mode, format) in cases {
            let mut snapshot = list_snapshot(mode, format, requested.to_vec());
            for (point, frequency) in snapshot.data.points.iter_mut().zip(actual) {
                point.freq_hz = frequency;
            }
            let expected = snapshot.clone();
            let frozen = FrozenSnapshots::new(vec![(TraceId(4), Arc::new(snapshot))]).unwrap();
            let csv = frozen.csv_bytes().unwrap();
            let mut reader = csv::Reader::from_reader(csv.as_slice());
            assert_eq!(
                reader.headers().unwrap().get(19),
                Some(REQUESTED_FREQUENCY_HEADER)
            );
            let columns = table::columns(mode, format).unwrap();
            let records: Vec<_> = reader.records().map(Result::unwrap).collect();
            assert_eq!(records.len(), requested.len() * columns.len());
            let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
            let data = xlsx.worksheet_range("T4").unwrap();
            let metadata = xlsx.worksheet_range("Metadata").unwrap();
            assert_eq!(data.get_size(), (4, columns.len() + 2));
            let requested_column = columns.len() as u32 + 1;
            assert_eq!(
                data.get_value((0, requested_column)),
                Some(&Data::String(REQUESTED_FREQUENCY_HEADER.into()))
            );
            for index in 0..requested.len() {
                assert_eq!(
                    data.get_value((index as u32 + 1, 0)),
                    Some(&Data::Float(actual[index]))
                );
                assert_eq!(
                    data.get_value((index as u32 + 1, requested_column)),
                    Some(&Data::Float(requested[index] as f64))
                );
                for (column, value) in expected.data.points[index].values.iter().enumerate() {
                    let record = &records[index * columns.len() + column];
                    assert_eq!(&record[1], index.to_string());
                    assert_eq!(record[2].parse::<f64>().unwrap(), actual[index]);
                    assert_eq!(&record[13], requested[0].to_string());
                    assert_eq!(&record[14], requested[2].to_string());
                    assert_eq!(&record[15], "3");
                    assert_eq!(&record[18], "list");
                    assert_eq!(record[19].parse::<u64>().unwrap(), requested[index]);
                    assert_eq!(record[4].parse::<f64>().unwrap(), *value);
                    assert_eq!(
                        data.get_value((index as u32 + 1, column as u32 + 1)),
                        Some(&Data::Float(*value))
                    );
                }
            }
            assert_eq!(
                metadata.get_value((1, 8)),
                Some(&Data::String(requested[0].to_string()))
            );
            assert_eq!(
                metadata.get_value((1, 9)),
                Some(&Data::String(requested[2].to_string()))
            );
            assert_eq!(
                metadata.get_value((1, 13)),
                Some(&Data::String("list".into()))
            );
            assert_no_formulas(&mut xlsx);
        }
    }

    #[test]
    fn zero_span_lists_and_nonfinite_values_remain_exportable_without_relabeling() {
        let mut snapshot = list_snapshot(StreamMode::S11, "z", vec![1_000_000; 3]);
        let actual = [1_000_000.25, 1_000_000.25, 999_999.875];
        for (point, frequency) in snapshot.data.points.iter_mut().zip(actual) {
            point.freq_hz = frequency;
        }
        snapshot.data.points[0].values = vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        let frozen = FrozenSnapshots::new(vec![(TraceId(1), Arc::new(snapshot))]).unwrap();
        let csv = frozen.csv_bytes().unwrap();
        let records: Vec<_> = csv::Reader::from_reader(csv.as_slice())
            .records()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            [&records[0][4], &records[1][4], &records[2][4]],
            ["NaN", "+Inf", "-Inf"]
        );
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        let data = xlsx.worksheet_range("T1").unwrap();
        for (index, frequency) in actual.into_iter().enumerate() {
            assert_eq!(
                data.get_value((index as u32 + 1, 0)),
                Some(&Data::Float(frequency))
            );
            assert_eq!(
                data.get_value((index as u32 + 1, 4)),
                Some(&Data::Float(1_000_000.0))
            );
        }
        for (index, value) in ["NaN", "+Inf", "-Inf"].into_iter().enumerate() {
            assert_eq!(
                data.get_value((1, index as u32 + 1)),
                Some(&Data::String(value.into()))
            );
        }
        assert_no_formulas(&mut xlsx);
    }

    #[test]
    fn list_exports_reject_mismatched_or_invalid_captured_definitions() {
        let valid = list_snapshot(
            StreamMode::S21,
            "delay",
            vec![1_000_000, 1_000_000, 1_300_007],
        );
        for change in [
            |snapshot: &mut CompletedSweep| {
                snapshot.data.points.pop();
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.mode = StreamMode::S11;
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.format = "loss".into();
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.points[0].values.push(1.0);
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.points[0].freq_hz = f64::NAN;
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.points[0].freq_hz = f64::INFINITY;
            },
            |snapshot: &mut CompletedSweep| {
                snapshot.data.points[0].freq_hz = -1.0;
            },
            |snapshot: &mut CompletedSweep| {
                if let AcquisitionSettings::List { frequencies_hz, .. } = &mut snapshot.settings {
                    frequencies_hz.swap(0, 2);
                }
            },
            |snapshot: &mut CompletedSweep| {
                if let AcquisitionSettings::List { frequencies_hz, .. } = &mut snapshot.settings {
                    frequencies_hz[0] = u64::MAX;
                }
            },
            |snapshot: &mut CompletedSweep| {
                if let AcquisitionSettings::List {
                    settings: PointSettings::S21 { rbw, .. },
                    ..
                } = &mut snapshot.settings
                {
                    *rbw = None;
                }
            },
        ] {
            let mut invalid = valid.clone();
            change(&mut invalid);
            assert!(FrozenSnapshots::new(vec![(TraceId(1), Arc::new(invalid))]).is_err());
        }
    }

    #[test]
    fn numeric_cells_keep_non_round_frequencies_and_full_float_significands() {
        let mut snapshot = snapshot(StreamMode::S21, "delay", 3);
        let frequencies = [
            1_000_000.123_456_789,
            1_500_000.987_654_321,
            2_000_000.333_333_333,
        ];
        let values = [
            -1.234_567_890_123_456_7e-12,
            f64::from_bits(1),
            9.876_543_210_987_654e-15,
        ];
        for (index, point) in snapshot.data.points.iter_mut().enumerate() {
            point.freq_hz = frequencies[index];
            point.values[0] = values[index];
        }
        let frozen = FrozenSnapshots::new(vec![(TraceId(4), Arc::new(snapshot))]).unwrap();
        let csv = frozen.csv_bytes().unwrap();
        for (index, record) in csv::Reader::from_reader(csv.as_slice())
            .records()
            .enumerate()
        {
            let record = record.unwrap();
            assert_eq!(
                record[2].parse::<f64>().unwrap().to_bits(),
                frequencies[index].to_bits()
            );
            assert_eq!(
                record[4].parse::<f64>().unwrap().to_bits(),
                values[index].to_bits()
            );
        }
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        let data = xlsx.worksheet_range("T4").unwrap();
        for index in 0..3 {
            for (column, expected) in [(0, frequencies[index]), (1, values[index])] {
                let Some(Data::Float(actual)) = data.get_value((index as u32 + 1, column)) else {
                    panic!("finite measurement was not a numeric cell");
                };
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
    }

    #[test]
    fn later_trace_replacement_does_not_change_frozen_export_or_metadata() {
        let mut live = Arc::new(snapshot(StreamMode::S21, "delay", 3));
        let original = live.clone();
        let frozen = FrozenSnapshots::new(vec![(TraceId(3), live.clone())]).unwrap();
        let changed = Arc::make_mut(&mut live);
        changed.data.points[0].values[0] = 999.0;
        changed.data.points[0].freq_hz = 0.0;
        changed.session_id = 100;
        changed.completed_at += Duration::from_secs(60);
        if let AcquisitionSettings::S21(params) = &mut changed.settings {
            params.cal = Cal::CalOff;
            params.lo = Lo::HighLo;
        }
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        let data = xlsx.worksheet_range("T3").unwrap();
        assert_eq!(
            data.get_value((1, 0)),
            Some(&Data::Float(original.data.points[0].freq_hz))
        );
        assert_eq!(
            data.get_value((1, 1)),
            Some(&Data::Float(original.data.points[0].values[0]))
        );
        let metadata = xlsx.worksheet_range("Metadata").unwrap();
        assert_eq!(
            metadata.get_value((1, 4)),
            Some(&Data::String("calsys".into()))
        );
        assert_eq!(
            metadata.get_value((1, 6)),
            Some(&Data::String("lowlo".into()))
        );
        assert_eq!(
            metadata.get_value((1, 12)),
            Some(&Data::String(original.session_id.to_string()))
        );
    }

    #[test]
    fn invalid_identities_settings_rows_and_frequencies_are_rejected_before_serialization() {
        let valid = Arc::new(snapshot(StreamMode::S11, "ri", 3));
        assert!(FrozenSnapshots::new(Vec::new()).is_err());
        assert!(FrozenSnapshots::new(vec![(TraceId(0), valid.clone())]).is_err());
        assert!(
            FrozenSnapshots::new(vec![
                (TraceId(1), valid.clone()),
                (TraceId(1), valid.clone())
            ])
            .is_err()
        );
        assert!(
            FrozenSnapshots::new((1..=11).map(|id| (TraceId(id), valid.clone())).collect())
                .is_err()
        );
        let check = |change: fn(&mut CompletedSweep)| {
            let mut invalid = (*valid).clone();
            change(&mut invalid);
            let error = FrozenSnapshots::new(vec![(TraceId(29), Arc::new(invalid))]).unwrap_err();
            assert!(error.starts_with("T29:"), "{error}");
        };
        for change in [
            |s: &mut CompletedSweep| s.session_id = 0,
            |s: &mut CompletedSweep| s.data.mode = StreamMode::S21,
            |s: &mut CompletedSweep| {
                s.data.format = "=HYPERLINK(\"https://example.invalid\")".into()
            },
            |s: &mut CompletedSweep| {
                s.data.points.pop();
            },
            |s: &mut CompletedSweep| s.data.points[0].values.clear(),
            |s: &mut CompletedSweep| s.data.points[0].values.push(0.0),
            |s: &mut CompletedSweep| s.data.points[0].freq_hz = f64::NAN,
            |s: &mut CompletedSweep| s.data.points[0].freq_hz = f64::INFINITY,
            |s: &mut CompletedSweep| s.data.points[0].freq_hz = -1.0,
            |s: &mut CompletedSweep| s.data.points.swap(0, 2),
            |s: &mut CompletedSweep| {
                if let AcquisitionSettings::S11(p) = &mut s.settings {
                    p.rbw = None;
                }
            },
            |s: &mut CompletedSweep| {
                if let AcquisitionSettings::S11(p) = &mut s.settings {
                    p.points = 1002;
                }
            },
            |s: &mut CompletedSweep| {
                if let AcquisitionSettings::S11(p) = &mut s.settings {
                    p.format = Format::Delay;
                }
            },
        ] {
            check(change);
        }
    }

    #[test]
    fn ten_full_size_traces_are_bounded_and_independently_readable() {
        let traces: Vec<_> = (1..=10)
            .map(|id| (TraceId(id), Arc::new(snapshot(StreamMode::S11, "z", 1001))))
            .collect();
        let frozen = FrozenSnapshots::new(traces).unwrap();
        assert_eq!(frozen.len(), 10);
        assert_eq!(frozen.point_count(), 10_010);
        let csv = frozen.csv_bytes().unwrap();
        assert_eq!(
            csv::Reader::from_reader(csv.as_slice())
                .records()
                .map(Result::unwrap)
                .count(),
            30_030
        );
        let mut xlsx = workbook(frozen.xlsx_bytes().unwrap());
        for id in 1..=10 {
            let data = xlsx.worksheet_range(&format!("T{id}")).unwrap();
            assert_eq!(data.get_size(), (1002, 4));
            assert_eq!(data.get_value((1001, 0)), Some(&Data::Float(2_000_000.25)));
        }
        assert_no_formulas(&mut xlsx);
    }

    #[test]
    fn timestamps_keep_subsecond_precision_on_both_sides_of_the_epoch() {
        assert_eq!(unix_timestamp(UNIX_EPOCH), "0.000000000");
        assert_eq!(
            unix_timestamp(UNIX_EPOCH + Duration::new(2, 100)),
            "2.000000100"
        );
        assert_eq!(
            unix_timestamp(UNIX_EPOCH - Duration::new(0, 100)),
            "-0.000000100"
        );
        assert_eq!(
            unix_timestamp(UNIX_EPOCH - Duration::new(2, 100)),
            "-2.000000100"
        );
    }
}
