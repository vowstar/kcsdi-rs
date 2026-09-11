// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use super::*;
use crate::acquisition::TraceId;
use crate::analysis_tools::{AnalysisConfig, AnalysisTools};
use crate::recording::{RecordKey, RecordWriter};
use crate::run_settings::{RecordingFormat, RecordingSettings};
use crate::spreadsheet::FrozenSnapshots;
use crate::widgets::plot::{self, CartesianLayer, Marker, PlotOptions, PlotView};
use std::time::Instant;

struct DelayedTail {
    peer: Peer,
    until: Option<Instant>,
}

impl Transport for DelayedTail {
    fn send_with_timeout(&mut self, bytes: &[u8], timeout: Duration) -> Result<()> {
        if bytes == b"\x03" {
            self.until = Some(Instant::now() + Duration::from_millis(2200));
        }
        if bytes.windows(5).any(|part| part == b",run,") {
            assert!(
                self.until.is_none(),
                "new run before residual data was drained"
            );
        }
        self.peer.send_with_timeout(bytes, timeout)
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<String> {
        if let Some(until) = self.until {
            if let Some(left) = until.checked_duration_since(Instant::now()) {
                std::thread::sleep(left.min(timeout));
            }
            if Instant::now() < until {
                return Err(Error::Timeout);
            }
            self.until = None;
        }
        self.peer.recv_line(timeout)
    }
}

#[test]
fn second_segment_slow_cancel_fences_the_next_complete_acquisition() {
    let plan = example_plan(&PointSettings::Spec {
        cal: Cal::CalOff,
        rbw: Rbw::R1k,
        lo: Lo::HighLo,
        ref_level_dbm: -10,
    });
    let mut peer = Peer::new(&plan);
    let mut following = Peer::new(&plan);
    for frame in &mut following.frames {
        for line in frame {
            if let Some(frequency) = line.strip_suffix(",-10") {
                *line = format!("{frequency},-20");
            }
        }
    }
    peer.frames.extend(following.frames);
    let sent = peer.sent.clone();
    let mut device = Device::new(DelayedTail { peer, until: None });
    let cancel = CancellationToken::default();
    let mut cancelled_at = None;
    let result = acquire(&mut device, &plan, &cancel, |_, progress| {
        if progress.index == 1 && progress.segment_points == 0 {
            assert_eq!(progress.acquired, 1001);
            cancelled_at = Some(Instant::now());
            cancel.cancel();
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(cancelled_at.unwrap().elapsed() >= Duration::from_millis(2200));
    assert!(!device.requires_reconnect());
    assert_eq!(
        sent.lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(",run,"))
            .count(),
        2
    );

    let (data, metadata) =
        acquire(&mut device, &plan, &CancellationToken::default(), |_, _| {}).unwrap();
    metadata.validate(&plan, &data).unwrap();
    assert_eq!(data.points.len(), 1951);
    assert!(data.points.iter().all(|point| point.values == [-20.0]));
    assert_eq!(metadata.received, [1001, 951]);
    assert_eq!(metadata.origins[1000].segment, 0);
    assert_eq!(metadata.origins[1001].point, 1);
    let sent = sent.lock().unwrap();
    let runs: Vec<_> = sent
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(",run,"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(runs.len(), 4);
    let abort = sent.iter().position(|line| line == "\x03").unwrap();
    let identity = sent.iter().position(|line| line == "$device\n").unwrap();
    assert!(runs[1] < abort && abort < identity && identity < runs[2]);
}

#[test]
fn nonuniform_markers_holds_and_recording_use_completed_raw_rows() {
    let first = example_snapshot();
    let mut analysis = AnalysisTools::default();
    analysis.restore_config(&AnalysisConfig {
        hold: true,
        max_hold: true,
        min_hold: true,
        markers: vec![Marker {
            id: 1,
            frequency_hz: 50_000_100.0,
            ..Marker::default()
        }],
        ..AnalysisConfig::default()
    });
    analysis.observe(&first);
    assert_eq!(analysis.markers()[0].frequency_hz, 50_000_000.0);
    assert_eq!(analysis.marker_frequencies(&first.data)[1001], 51_000_000.0);
    let mut next = first.clone();
    for point in &mut next.data.points {
        point.values = vec![70.0, 50.0, -40.0];
    }
    analysis.observe(&next);
    assert_eq!(
        analysis.held_trace().unwrap().points[1001].values,
        [50.0, 40.0, -30.0]
    );
    let overlays = analysis.overlay_series(&[0, 1, 2]);
    assert_eq!(overlays.len(), 9);
    assert_eq!(overlays[3].points[1001], (51_000_000.0, 70.0));
    assert_eq!(overlays[8].points[1001], (51_000_000.0, -40.0));

    // An edited definition resets holds even when its calculated grid is unchanged.
    let AcquisitionSettings::Segments(plan) = &next.settings else {
        unreachable!()
    };
    let mut rows: Vec<_> = plan.segments().iter().map(|row| row.definition()).collect();
    rows[0].max_step_hz += 1;
    next.settings = SegmentPlan::new(&rows, plan.settings(), &Model::Kc901V.capabilities())
        .unwrap()
        .into();
    assert!(next.accepts_data());
    analysis.observe(&next);
    assert_eq!(
        analysis.held_trace().unwrap().points[1001].values,
        [70.0, 50.0, -40.0]
    );

    let frozen = FrozenSnapshots::new(vec![(TraceId(1), Arc::new(next))]).unwrap();
    let expected = frozen.csv_bytes().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let settings = RecordingSettings {
        enabled: true,
        directory: directory.path().into(),
        format: RecordingFormat::Csv,
        ..RecordingSettings::default()
    };
    let mut writer = RecordWriter::new().unwrap();
    for (pass_id, cancelled) in [(1, false), (2, true)] {
        let cancel = CancellationToken::default();
        if cancelled {
            cancel.cancel();
        }
        let key = RecordKey {
            session_id: 1,
            request_id: 1,
            pass_id,
        };
        writer
            .try_save(key, settings.clone(), frozen.clone(), cancel)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            if let Some(result) = writer.poll() {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(result.key, key);
        let saved = result.result.unwrap();
        if cancelled {
            assert!(saved.is_none());
        } else {
            assert_eq!(std::fs::read(saved.unwrap()).unwrap(), expected);
        }
    }
}

/// Explicit resource exercise, not a timing assertion on shared CI runners.
#[test]
#[ignore = "run explicitly to measure ten maximum-size segmented acquisitions"]
fn maximum_segment_budget_acquisition_analysis_plot_and_exports() {
    use calamine::{Reader, Xlsx, open_workbook_from_rs};
    use kcsdi_core::segments::{MAX_ACQUIRED_POINTS, MAX_SEGMENTS};
    use std::io::Cursor;

    let retained = MAX_ACQUIRED_POINTS as usize - (MAX_SEGMENTS - 1);
    let definitions: Vec<_> = (0..MAX_SEGMENTS)
        .map(|index| Segment {
            start_hz: 5000 + index as u64 * 1_000_000,
            stop_hz: 5000 + (index as u64 + 1) * 1_000_000,
            max_step_hz: 1000,
        })
        .collect();
    let acquired = Instant::now();
    let mut snapshots = Vec::new();
    for index in 0..10 {
        let receiver = PointSettings::S11 {
            cal: [Cal::CalOff, Cal::CalSys, Cal::CalUser][index / 4],
            rbw: Some([Rbw::R1k, Rbw::R3k, Rbw::R10k, Rbw::R30k][index % 4]),
            format: Format::Z,
        };
        let plan =
            SegmentPlan::new(&definitions, &receiver, &Model::Kc901V.capabilities()).unwrap();
        assert_eq!(plan.acquired_points(), MAX_ACQUIRED_POINTS);
        let peer = Peer::new(&plan);
        let sent = peer.sent.clone();
        let mut device = Device::new(peer);
        let mut previews = 0;
        let (data, segments) = acquire(
            &mut device,
            &plan,
            &CancellationToken::default(),
            |prefix, progress| {
                assert!(prefix.points.len() <= MAX_ACQUIRED_POINTS as usize);
                assert!(progress.acquired <= MAX_ACQUIRED_POINTS);
                previews += 1;
            },
        )
        .unwrap();
        assert!(previews >= MAX_SEGMENTS * 2);
        assert_eq!(
            sent.lock()
                .unwrap()
                .iter()
                .filter(|command| command.contains(",run,"))
                .count(),
            MAX_SEGMENTS
        );
        let snapshot = CompletedSweep {
            data,
            segments: Some(segments),
            settings: plan.into(),
            session_id: 1,
            completed_at: SystemTime::UNIX_EPOCH,
        };
        assert!(snapshot.accepts_data());
        assert_eq!(snapshot.data.points.len(), retained);
        snapshots.push((TraceId(index as u64 + 1), Arc::new(snapshot)));
    }
    println!("acquisition_ms={}", acquired.elapsed().as_millis());
    let observed = Instant::now();
    let mut analyses: Vec<_> = snapshots
        .iter()
        .map(|(_, snapshot)| {
            let mut analysis = AnalysisTools::default();
            analysis.restore_config(&AnalysisConfig {
                hold: true,
                max_hold: true,
                min_hold: true,
                ..AnalysisConfig::default()
            });
            analysis.observe(snapshot);
            analysis.observe(snapshot);
            analysis
        })
        .collect();
    println!("analysis_ms={}", observed.elapsed().as_millis());

    let context = egui::Context::default();
    crate::theme::setup(&context);
    for frame in 0..3 {
        let plotted = Instant::now();
        let stop = definitions.last().unwrap().stop_hz as f64;
        let mut views = [PlotView::new(5000.0, stop, -100.0, 100.0); 10];
        let mut markers = vec![Vec::new(); 10];
        let mut layers: Vec<_> = views
            .iter_mut()
            .zip(&mut analyses)
            .zip(&mut markers)
            .enumerate()
            .map(|(index, ((view, analysis), markers))| CartesianLayer {
                id: index as u64 + 1,
                label: format!("T{}", index + 1),
                view,
                options: PlotOptions {
                    y_label: "ohm",
                    log_x: true,
                    series: ["|Z|", "R", "X"]
                        .into_iter()
                        .enumerate()
                        .map(|(column, name)| plot::Series {
                            name,
                            color: egui::Color32::YELLOW,
                            visible: true,
                            points: snapshots[index]
                                .1
                                .data
                                .points
                                .iter()
                                .map(|point| (point.freq_hz, point.values[column]))
                                .collect(),
                        })
                        .chain(analysis.overlay_series(&[0, 1, 2]))
                        .collect(),
                },
                markers,
                marker_frequencies: &[],
                line_width: 1.0,
            })
            .collect();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| {
                plot::show_multi(ui, &mut layers, Some(1));
            },
        );
        let shapes = output.shapes.len();
        let primitives = context.tessellate(output.shapes.clone(), output.pixels_per_point);
        assert!(!primitives.is_empty());
        output.drop_without_applying_deltas();
        println!(
            "plot_frame={frame} frame_ms={} shapes={shapes}",
            plotted.elapsed().as_millis()
        );
    }
    let frozen = FrozenSnapshots::new(snapshots).unwrap();
    assert_eq!(frozen.point_count(), retained * 10);
    let serialized = Instant::now();
    let csv = frozen.csv_bytes().unwrap();
    println!(
        "csv_ms={} csv_bytes={}",
        serialized.elapsed().as_millis(),
        csv.len()
    );
    assert_eq!(
        csv::Reader::from_reader(csv.as_slice())
            .records()
            .try_fold(0, |count, row| row.map(|_| count + 1))
            .unwrap(),
        retained * 10 * 3
    );
    drop(csv);
    let serialized = Instant::now();
    let xlsx = frozen.xlsx_bytes().unwrap();
    println!(
        "xlsx_ms={} xlsx_bytes={}",
        serialized.elapsed().as_millis(),
        xlsx.len()
    );
    let mut book: Xlsx<_> = open_workbook_from_rs(Cursor::new(xlsx)).unwrap();
    for id in 1..=10 {
        assert_eq!(
            book.worksheet_range(&format!("T{id}")).unwrap().height(),
            retained + 1
        );
    }
    assert_eq!(
        book.worksheet_range("Segments").unwrap().height(),
        MAX_SEGMENTS * 10 + 1
    );
}
