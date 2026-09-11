// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::acquisition::{AcquisitionSettings, CompletedSweep};
use kcsdi_core::commands::{Cal, Format, Lo};
use kcsdi_core::data::SweepPoint;
use kcsdi_core::device::PointSettings;
use kcsdi_core::model::{Model, Rbw};
use kcsdi_core::segments::{PlannedSegment, Segment};

use super::*;

mod workflow;

fn receiver(mode: &str, format: Format) -> PointSettings {
    match mode {
        "s11" => PointSettings::S11 {
            cal: Cal::CalOff,
            format,
            rbw: Some(Rbw::R10k),
        },
        "s21" => PointSettings::S21 {
            cal: Cal::CalOff,
            format,
            rbw: Some(Rbw::R10k),
            lo: Lo::HighLo,
        },
        _ => PointSettings::Spec {
            cal: Cal::CalOff,
            rbw: Rbw::R10k,
            lo: Lo::HighLo,
            ref_level_dbm: -10,
        },
    }
}

fn small_plan(settings: &PointSettings) -> SegmentPlan {
    SegmentPlan::new(
        &[
            Segment {
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                max_step_hz: 500_000,
            },
            Segment {
                start_hz: 2_000_000,
                stop_hz: 3_000_000,
                max_step_hz: 500_000,
            },
        ],
        settings,
        &Model::Kc901V.capabilities(),
    )
    .unwrap()
}

fn frame(row: &PlannedSegment, settings: &PointSettings) -> SweepData {
    let values = match settings.format() {
        "z" => vec![50.0, 40.0, -30.0],
        "ri" => vec![0.1, -0.2],
        "ma" => vec![0.3, -45.0],
        "vswr" => vec![1.3],
        "delay" => vec![-1e-9],
        _ => vec![-10.0],
    };
    SweepData {
        mode: settings.mode(),
        format: settings.format().into(),
        points: (0..row.points())
            .map(|index| SweepPoint {
                freq_hz: row.definition().start_hz as f64 + f64::from(index) * row.step_hz(),
                values: values.clone(),
            })
            .collect(),
    }
}

fn example_plan(settings: &PointSettings) -> SegmentPlan {
    SegmentPlan::new(
        &[
            Segment {
                start_hz: 5000,
                stop_hz: 50_000_000,
                max_step_hz: 50_000,
            },
            Segment {
                start_hz: 50_000_000,
                stop_hz: 1_000_000_000,
                max_step_hz: 1_000_000,
            },
        ],
        settings,
        &Model::Kc901V.capabilities(),
    )
    .unwrap()
}

pub(crate) fn example_snapshot() -> CompletedSweep {
    let settings = receiver("s11", Format::Z);
    let plan = example_plan(&settings);
    let mut joiner = SegmentJoiner::new(plan.clone());
    for row in plan.segments() {
        joiner.append(frame(row, &settings)).unwrap();
    }
    let (data, segments) = joiner.finish().unwrap();
    CompletedSweep {
        data,
        segments: Some(segments),
        settings: AcquisitionSettings::from(plan),
        session_id: 1,
        completed_at: SystemTime::UNIX_EPOCH,
    }
}

struct Peer {
    frames: VecDeque<Vec<String>>,
    incoming: VecDeque<String>,
    sent: Arc<Mutex<Vec<String>>>,
}

impl Peer {
    fn new(plan: &SegmentPlan) -> Self {
        let frames = plan
            .segments()
            .iter()
            .map(|row| {
                let data = frame(row, plan.settings());
                let header = if data.format.is_empty() {
                    format!("$start,{}", data.mode.name())
                } else {
                    format!("$start,{},{}", data.mode.name(), data.format)
                };
                std::iter::once(header)
                    .chain(data.points.iter().map(|point| {
                        format!(
                            "${},{}",
                            point.freq_hz,
                            point
                                .values
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    }))
                    .chain(std::iter::once("$end".into()))
                    .collect()
            })
            .collect();
        Self {
            frames,
            incoming: VecDeque::new(),
            sent: Arc::default(),
        }
    }
}

impl Transport for Peer {
    fn send_with_timeout(&mut self, bytes: &[u8], _: Duration) -> Result<()> {
        let command = String::from_utf8(bytes.to_vec()).unwrap();
        if command.contains(",run,") {
            assert!(
                self.incoming.is_empty(),
                "next run before the previous frame ended"
            );
            self.incoming.extend(
                self.frames
                    .pop_front()
                    .expect("unexpected extra acquisition"),
            );
        }
        if command == "$device\n" {
            self.incoming.extend(
                [
                    "$start,device",
                    "$Synthetic peer",
                    "$<-User @ :replay>",
                    "$<-Software ver:test>",
                    "$<-Hardware ver:test>",
                    "$<-Serial num:000000000001>",
                    "$<-Copyright:Test fixture>",
                    "$end",
                ]
                .map(str::to_owned),
            );
        }
        self.sent.lock().unwrap().push(command);
        Ok(())
    }
    fn recv_line(&mut self, _: Duration) -> Result<String> {
        self.incoming.pop_front().ok_or(Error::Timeout)
    }
}

#[test]
fn every_receiver_format_uses_sequential_finite_frames_and_whole_result_metadata() {
    for (mode, format) in [
        ("s11", Format::Ri),
        ("s11", Format::Ma),
        ("s11", Format::Vswr),
        ("s11", Format::Loss),
        ("s11", Format::Z),
        ("s21", Format::Ri),
        ("s21", Format::Ma),
        ("s21", Format::Loss),
        ("s21", Format::Delay),
        ("spec", Format::Loss),
    ] {
        let plan = small_plan(&receiver(mode, format));
        let peer = Peer::new(&plan);
        let sent = peer.sent.clone();
        let mut device = Device::new(peer);
        let mut previews = Vec::new();
        let (data, metadata) = acquire(
            &mut device,
            &plan,
            &CancellationToken::default(),
            |prefix, progress| {
                assert_eq!(prefix.expected_points, 6);
                assert_eq!(progress.expected, 6);
                previews.push((prefix.points.len(), progress));
            },
        )
        .unwrap();
        assert_eq!(data.points.len(), 5);
        metadata.validate(&plan, &data).unwrap();
        assert!(previews.iter().any(|(count, progress)| *count == 3
            && progress.index == 1
            && progress.segment_points == 0));
        assert_eq!(previews.last().unwrap().1.acquired, 6);
        assert_eq!(metadata.received, [3, 3]);
        let sent = sent.lock().unwrap();
        let runs: Vec<_> = sent
            .iter()
            .filter(|command| command.contains(",run,"))
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].ends_with(",2,ss,1000000,2000000\n"));
        assert!(runs[1].ends_with(",2,ss,2000000,3000000\n"));
    }
}

#[test]
fn cancel_inside_or_between_segments_never_returns_partial_completion() {
    for (segment, point) in [(0, 1), (0, 3), (1, 1)] {
        let plan = small_plan(&receiver("s11", Format::Z));
        let peer = Peer::new(&plan);
        let sent = peer.sent.clone();
        let mut device = Device::new(peer);
        let cancel = CancellationToken::default();
        let result = acquire(&mut device, &plan, &cancel, |_, progress| {
            if progress.index == segment && progress.segment_points >= point {
                cancel.cancel();
            }
        });
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(!device.requires_reconnect());
        assert_eq!(
            sent.lock()
                .unwrap()
                .iter()
                .filter(|command| command.contains(",run,"))
                .count(),
            segment + 1
        );
    }
}

#[test]
fn later_error_and_frame_restart_never_mix_incomplete_rows() {
    let plan = small_plan(&receiver("s11", Format::Z));
    let mut peer = Peer::new(&plan);
    peer.frames[1] = vec!["$start,err_par5".into(), "$invalid".into(), "$end".into()];
    let mut device = Device::new(peer);
    assert!(matches!(
        acquire(&mut device, &plan, &CancellationToken::default(), |_, _| {}),
        Err(Error::Device(_))
    ));

    let mut peer = Peer::new(&plan);
    peer.frames[1].splice(0..0, ["$start,s11,z".into(), "$2000000,999,999,999".into()]);
    let mut device = Device::new(peer);
    let (data, metadata) =
        acquire(&mut device, &plan, &CancellationToken::default(), |_, _| {}).unwrap();
    metadata.validate(&plan, &data).unwrap();
    assert!(
        data.points
            .iter()
            .all(|point| point.values == [50.0, 40.0, -30.0])
    );
}

#[test]
fn completed_snapshot_rejects_missing_truncated_or_forged_provenance() {
    let snapshot = example_snapshot();
    assert_eq!(snapshot.data.points.len(), 1951);
    assert!(snapshot.accepts_data());
    let mut broken = snapshot.clone();
    broken.segments = None;
    assert!(!broken.accepts_data());
    let mut broken = snapshot.clone();
    broken.data.points.pop();
    broken.segments.as_mut().unwrap().origins.pop();
    assert!(!broken.accepts_data());
    let mut broken = snapshot.clone();
    broken.segments.as_mut().unwrap().origins[0].point = 1;
    assert!(!broken.accepts_data());
    let mut broken = snapshot;
    broken.segments.as_mut().unwrap().received[1] -= 1;
    assert!(!broken.accepts_data());
}

#[test]
fn large_nonuniform_exports_keep_frequency_values_and_segment_definitions() {
    use crate::acquisition::TraceId;
    use crate::spreadsheet::FrozenSnapshots;
    use calamine::{Data, Reader, Xlsx, open_workbook_from_rs};
    use kcsdi_core::touchstone::{Document, Version};
    use std::io::Cursor;

    let snapshot = Arc::new(example_snapshot());
    let frozen = FrozenSnapshots::new(vec![(TraceId(1), snapshot.clone())]).unwrap();
    let bytes = frozen.csv_bytes().unwrap();
    let mut reader = csv::Reader::from_reader(bytes.as_slice());
    let headers = reader.headers().unwrap().clone();
    let column = |name| headers.iter().position(|header| header == name).unwrap();
    let records: Vec<_> = reader.records().map(|row| row.unwrap()).collect();
    assert_eq!(records.len(), 1951 * 3);
    for (index, point) in snapshot.data.points.iter().enumerate() {
        for (value, row) in point.values.iter().zip(&records[index * 3..index * 3 + 3]) {
            assert_eq!(
                row[column("freq_hz")].parse::<f64>().unwrap(),
                point.freq_hz
            );
            assert_eq!(row[column("value")].parse::<f64>().unwrap(), *value);
            assert_eq!(&row[column("frequency_plan")], "segments");
            assert_eq!(&row[column("requested_points")], "1952");
        }
    }
    assert_eq!(&records[1000 * 3][column("segment_index")], "0");
    assert_eq!(&records[1001 * 3][column("segment_index")], "1");
    assert_eq!(&records[1001 * 3][column("segment_point_index")], "1");
    assert_eq!(&records[1001 * 3][column("segment_retained_points")], "950");

    let mut workbook: Xlsx<_> =
        open_workbook_from_rs(Cursor::new(frozen.xlsx_bytes().unwrap())).unwrap();
    let data = workbook.worksheet_range("T1").unwrap();
    assert_eq!(data.height(), 1952);
    assert_eq!(data.get_value((1002, 0)), Some(&Data::Float(51_000_000.0)));
    assert_eq!(data.get_value((1002, 4)), Some(&Data::Float(1.0)));
    let segments = workbook.worksheet_range("Segments").unwrap();
    assert_eq!(segments.height(), 3);
    assert_eq!(
        segments.get_value((1, 2)),
        Some(&Data::String("5000".into()))
    );
    assert_eq!(
        segments.get_value((2, 7)),
        Some(&Data::String("950".into()))
    );

    for version in [Version::V1, Version::V2] {
        let document = Document::s1p(&snapshot.data, version).unwrap();
        let rows: Vec<_> = document
            .as_str()
            .lines()
            .filter(|line| line.starts_with(|c: char| c.is_ascii_digit()))
            .collect();
        assert_eq!(rows.len(), 1951);
    }
}
