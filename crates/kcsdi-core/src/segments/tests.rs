// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use super::*;
use crate::commands::{Cal, Format, Lo};
use crate::data::SweepPoint;
use crate::model::{Model, Rbw};
use crate::protocol::StreamMode;

fn settings() -> PointSettings {
    PointSettings::S11 {
        cal: Cal::CalOff,
        format: Format::Z,
        rbw: Some(Rbw::R10k),
    }
}

fn segment(start_hz: u64, stop_hz: u64, max_step_hz: u64) -> Segment {
    Segment {
        start_hz,
        stop_hz,
        max_step_hz,
    }
}

fn plan(rows: &[Segment]) -> SegmentPlan {
    SegmentPlan::new(rows, &settings(), &Model::Kc901V.capabilities()).unwrap()
}

fn small_plan() -> SegmentPlan {
    plan(&[segment(5000, 6000, 500), segment(6000, 7000, 500)])
}

fn frame(frequencies: &[f64], value: f64) -> SweepData {
    SweepData {
        mode: StreamMode::S11,
        format: "z".into(),
        points: frequencies
            .iter()
            .map(|&freq_hz| SweepPoint {
                freq_hz,
                values: vec![value, -value, 0.0],
            })
            .collect(),
    }
}

#[test]
fn requested_example_uses_two_finite_commands_and_nominal_step_bounds() {
    let plan = plan(&[
        segment(5000, 50_000_000, 50_000),
        segment(50_000_000, 1_000_000_000, 1_000_000),
    ]);
    assert_eq!(plan.acquired_points(), 1952);
    assert_eq!(plan.segments()[0].points(), 1001);
    assert_eq!(plan.segments()[0].step_hz(), 49_995.0);
    assert_eq!(plan.segments()[1].points(), 951);
    let caps = Model::Kc901V.capabilities();
    assert_eq!(caps.wire_points(plan.segments()[0].points()).unwrap(), 1000);
    assert_eq!(caps.wire_points(plan.segments()[1].points()).unwrap(), 950);
    for row in plan.segments() {
        row.sweep(plan.settings()).validate(&caps).unwrap();
    }
}

#[test]
fn integer_planner_never_silently_coarsens_an_oversized_row() {
    let caps = Model::Kc901V.capabilities();
    assert_eq!(
        segment(5000, 1_000_000_000, 50_000).points(&caps),
        Err(SegmentProblem::Points {
            required: 20_001,
            maximum: 1001
        })
    );
    assert_eq!(
        segment(0, u64::MAX, 1).points(&caps),
        Err(SegmentProblem::Points {
            required: u128::from(u64::MAX) + 1,
            maximum: 1001
        })
    );
    assert_eq!(segment(5000, 6000, u64::MAX).points(&caps), Ok(3));
    assert_eq!(segment(5000, 6001, 500).points(&caps), Ok(4));
    for span in 1000..=2000 {
        for step in 1..=100 {
            let definition = segment(5000, 5000 + span, step);
            if let Ok(points) = definition.points(&caps) {
                let intervals = u64::from(points - 1);
                assert!(intervals * step >= span);
                assert!(intervals == 2 || (intervals - 1) * step < span);
            }
        }
    }
}

#[test]
fn plan_reports_the_affected_row_and_rejects_invalid_definitions() {
    let caps = Model::Kc901V.capabilities();
    for (rows, row, problem) in [
        (vec![], None, SegmentProblem::Count),
        (vec![segment(5000, 6000, 0)], Some(0), SegmentProblem::Step),
        (
            vec![segment(6000, 5000, 500)],
            Some(0),
            SegmentProblem::Order,
        ),
        (
            vec![segment(5000, 5000, 500)],
            Some(0),
            SegmentProblem::Order,
        ),
        (
            vec![segment(5000, 6000, 500), segment(6001, 8000, 500)],
            Some(1),
            SegmentProblem::Adjacency,
        ),
        (
            vec![segment(5000, 6000, 500), segment(5999, 8000, 500)],
            Some(1),
            SegmentProblem::Adjacency,
        ),
        (
            vec![segment(5000, 6000, 500); MAX_SEGMENTS + 1],
            None,
            SegmentProblem::Count,
        ),
    ] {
        assert_eq!(
            SegmentPlan::new(&rows, &settings(), &caps),
            Err(SegmentError { row, problem })
        );
    }
    for definition in [
        segment(0, 6000, 500),
        segment(5000, 5999, 500),
        segment(7_000_000_000, 7_000_001_000, 500),
    ] {
        let error = SegmentPlan::new(&[definition], &settings(), &caps).unwrap_err();
        assert_eq!(error.row, Some(0));
        assert!(matches!(error.problem, SegmentProblem::Receiver(_)));
    }
}

#[test]
fn all_receiver_fields_and_capability_point_conventions_are_validated() {
    let caps = Model::Kc901V.capabilities();
    let rows = [segment(1_000_000, 2_000_000, 1000)];
    for receiver in [
        PointSettings::S11 {
            cal: Cal::CalOn,
            format: Format::Z,
            rbw: Some(Rbw::R10k),
        },
        PointSettings::S21 {
            cal: Cal::CalOff,
            format: Format::Z,
            rbw: Some(Rbw::R10k),
            lo: Lo::HighLo,
        },
        PointSettings::Spec {
            cal: Cal::CalOff,
            rbw: Rbw::R100Hz,
            lo: Lo::HighLo,
            ref_level_dbm: 0,
        },
        PointSettings::Spec {
            cal: Cal::CalOff,
            rbw: Rbw::R1k,
            lo: Lo::HighLo,
            ref_level_dbm: 11,
        },
    ] {
        assert!(SegmentPlan::new(&rows, &receiver, &caps).is_err());
    }
    let modern = Model::Kc901K.capabilities();
    assert_eq!(
        segment(1_000_000, 2_000_000, 1000).points(&modern),
        Err(SegmentProblem::Points {
            required: 1001,
            maximum: 1000
        })
    );
    assert_eq!(
        segment(1_000_000, 2_000_000, u64::MAX).points(&modern),
        Ok(2)
    );
}

#[test]
fn maximum_plan_has_a_bounded_acquired_count() {
    let rows: Vec<_> = (0..MAX_SEGMENTS as u64)
        .map(|index| segment(5000 + index * 1000, 6000 + index * 1000, 1))
        .collect();
    assert_eq!(plan(&rows).acquired_points(), MAX_ACQUIRED_POINTS);
}

#[test]
fn exact_boundary_keeps_the_earlier_whole_row_and_its_origin() {
    let mut joiner = SegmentJoiner::new(small_plan());
    joiner
        .append(frame(&[5000.0, 5500.0, 6000.0], 1.0))
        .unwrap();
    joiner
        .append(frame(&[6000.0, 6500.0, 7000.0], 2.0))
        .unwrap();
    assert_eq!(joiner.acquired_points(), 6);
    let (data, metadata) = joiner.finish().unwrap();
    assert_eq!(data.points.len(), 5);
    assert_eq!(data.points[2].values, [1.0, -1.0, 0.0]);
    assert_eq!(metadata.received, [3, 3]);
    assert_eq!(
        metadata.origins[2],
        SampleOrigin {
            segment: 0,
            point: 2
        }
    );
    assert_eq!(
        metadata.origins[3],
        SampleOrigin {
            segment: 1,
            point: 1
        }
    );
}

#[test]
fn reported_endpoint_differences_are_retained_without_clamping() {
    for end in [5999.0, 6200.0] {
        let mut joiner = SegmentJoiner::new(small_plan());
        joiner.append(frame(&[5000.0, 5500.0, end], 1.0)).unwrap();
        joiner
            .append(frame(&[6000.0, 6500.0, 7000.0], 2.0))
            .unwrap();
        let (data, metadata) = joiner.finish().unwrap();
        assert_eq!(data.points.len(), 6);
        assert!(
            data.points
                .windows(2)
                .all(|pair| pair[0].freq_hz < pair[1].freq_hz)
        );
        let index = data
            .points
            .iter()
            .position(|point| point.freq_hz == end)
            .unwrap();
        assert_eq!(data.points[index].values, [1.0, -1.0, 0.0]);
        assert_eq!(
            metadata.origins[index],
            SampleOrigin {
                segment: 0,
                point: 2
            }
        );
    }
}

#[test]
fn failed_frames_do_not_advance_or_mutate_the_accepted_prefix() {
    let mut joiner = SegmentJoiner::new(small_plan());
    joiner
        .append(frame(&[5000.0, 5500.0, 6200.0], 1.0))
        .unwrap();
    let prefix = joiner.preview(&[]).unwrap();
    let mut bad_schema = frame(&[6000.0, 6500.0, 7000.0], 2.0);
    bad_schema.points[1].values.pop();
    let mut wrong_format = bad_schema.clone();
    wrong_format.format = "ri".into();
    for invalid in [
        frame(&[6000.0, 6100.0, 7000.0], 2.0),
        frame(&[6000.0, 6000.0, 7000.0], 2.0),
        frame(&[6000.0, 6500.0], 2.0),
        frame(&[6000.0, 6500.0, 7000.0, 7100.0], 2.0),
        frame(&[5500.0, 6500.0, 7000.0], 2.0),
        frame(&[6000.0, 6500.0, 7500.0], 2.0),
        frame(&[6000.0, f64::NAN, 7000.0], 2.0),
        bad_schema,
        wrong_format,
    ] {
        assert!(joiner.append(invalid).is_err());
        assert_eq!(joiner.acquired_points(), 3);
        assert_eq!(joiner.preview(&[]).unwrap(), prefix);
    }
    joiner
        .append(frame(&[6000.0, 6500.0, 7000.0], 2.0))
        .unwrap();
    assert!(
        joiner
            .append(frame(&[7000.0, 7500.0, 8000.0], 3.0))
            .is_err()
    );
    assert!(joiner.finish().is_ok());
}

#[test]
fn preview_replacements_do_not_duplicate_or_complete_a_segment() {
    let mut joiner = SegmentJoiner::new(small_plan());
    joiner
        .append(frame(&[5000.0, 5500.0, 6200.0], 1.0))
        .unwrap();
    let partial = frame(&[6000.0, 6500.0], 2.0);
    assert_eq!(joiner.preview(&partial.points[..1]).unwrap().len(), 4);
    assert_eq!(joiner.preview(&partial.points).unwrap().len(), 5);
    assert_eq!(joiner.preview(&[]).unwrap().len(), 3);
    assert_eq!(joiner.acquired_points(), 3);
    assert!(joiner.finish().is_err());
}

#[test]
fn raw_measurement_sentinels_are_not_changed_by_joining() {
    let mut joiner = SegmentJoiner::new(small_plan());
    joiner
        .append(frame(&[5000.0, 5500.0, 6000.0], f64::NAN))
        .unwrap();
    joiner
        .append(frame(&[6000.0, 6500.0, 7000.0], f64::INFINITY))
        .unwrap();
    let (data, _) = joiner.finish().unwrap();
    assert!(data.points[2].values[0].is_nan());
    assert_eq!(
        data.points[3].values,
        [f64::INFINITY, f64::NEG_INFINITY, 0.0]
    );
}
