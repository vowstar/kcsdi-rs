// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Cross-module frequency-list regression tests without a device connection.

use crate::acquisition::{AcquisitionSettings, SweepPlan, TraceId};
use crate::config::AppConfig;
use crate::state::{
    AppState, ConnectionState, DEVICE_MODEL, S11Display, S21Display, SweepState, WorkerCommand,
};
use crate::workspace::{SweepRange, TraceDisplay, TraceSettings, Workspace};

fn workspace(display: TraceDisplay, frequencies_hz: Vec<u64>) -> Workspace {
    let mut workspace = Workspace {
        list_mode: true,
        frequencies_hz,
        ..Default::default()
    };
    workspace.traces[0].settings.display = display;
    workspace
}

#[test]
fn grouping_includes_the_complete_list_and_keeps_receiver_and_mode_boundaries() {
    let mut workspace = workspace(
        TraceDisplay::S11(S11Display::Impedance),
        vec![1_000_000, 1_000_000, 2_000_000],
    );
    let smith = workspace
        .add_trace(TraceSettings {
            display: TraceDisplay::S11(S11Display::Smith),
            ..Default::default()
        })
        .unwrap();
    let spectrum = workspace.add_trace(TraceSettings::default()).unwrap();
    let plan = workspace.plan().unwrap();
    assert_eq!(plan.groups.len(), 2);
    assert_eq!(plan.groups[0].members, [TraceId(1), smith]);
    assert_eq!(plan.groups[1].members, [spectrum]);
    let AcquisitionSettings::List { frequencies_hz, .. } = &plan.groups[0].settings else {
        panic!("list was converted into finite settings");
    };
    assert_eq!(frequencies_hz, &[1_000_000, 1_000_000, 2_000_000]);
    workspace.traces[1].settings.rbw = kcsdi_core::model::Rbw::R1k;
    assert_eq!(workspace.plan().unwrap().groups.len(), 3);

    let mut changed = plan.groups[0].settings.clone();
    if let AcquisitionSettings::List { frequencies_hz, .. } = &mut changed {
        frequencies_hz[1] = 1_500_000;
    }
    let split = SweepPlan::from_requests([
        (TraceId(1), plan.groups[0].settings.clone()),
        (TraceId(2), changed),
    ])
    .unwrap();
    assert_eq!(split.groups.len(), 2);
}

#[test]
fn range_and_list_definitions_are_independent_and_never_share_a_group() {
    let mut workspace = workspace(
        TraceDisplay::S11(S11Display::Impedance),
        vec![1_000_000, 1_500_000, 2_000_000],
    );
    workspace.range = SweepRange::new(1_000_000.0, 2_000_000.0, 3);
    let listed = workspace.plan().unwrap().groups[0].settings.clone();
    workspace.list_mode = false;
    let ranged = workspace.plan().unwrap().groups[0].settings.clone();
    assert_ne!(listed, ranged);
    assert_eq!(
        SweepPlan::from_requests([(TraceId(1), listed.clone()), (TraceId(2), ranged)])
            .unwrap()
            .groups
            .len(),
        2
    );
    let list = workspace.frequencies_hz.clone();
    workspace.range = SweepRange::new(-1.0, f64::NAN, 0);
    assert!(workspace.plan().is_err());
    workspace.list_mode = true;
    assert_eq!(workspace.plan().unwrap().groups[0].settings, listed);
    assert_eq!(workspace.frequencies_hz, list);
    workspace.list_mode = false;
    workspace.range = SweepRange::new(1_000_000.0, 2_000_000.0, 3);
    workspace.frequencies_hz = vec![u64::MAX, 0];
    assert!(workspace.plan().is_ok());
    workspace.list_mode = true;
    assert!(workspace.plan().is_err());
}

#[test]
fn list_validation_uses_visible_point_limits_not_finite_span() {
    let caps = DEVICE_MODEL.capabilities();
    for (display, limits) in [
        (TraceDisplay::Spec, caps.spec.range),
        (TraceDisplay::S11(S11Display::Impedance), caps.s11.range),
        (TraceDisplay::S21(S21Display::Delay), caps.s21.range),
    ] {
        let mut workspace = workspace(display, vec![limits.min_hz, limits.min_hz, limits.max_hz]);
        assert!(
            workspace.plan().is_ok(),
            "point endpoints rejected for {display:?}"
        );
        workspace.frequencies_hz = vec![limits.min_hz; 3];
        assert!(
            workspace.plan().is_ok(),
            "equal points became a finite sweep"
        );
        workspace.frequencies_hz[2] = limits.max_hz + 1;
        assert!(workspace.plan().is_err());
    }
    let mut workspace = workspace(TraceDisplay::Spec, vec![0; 3]);
    let reflection = workspace
        .add_trace(TraceSettings {
            display: TraceDisplay::S11(S11Display::Impedance),
            visible: false,
            ..Default::default()
        })
        .unwrap();
    assert!(workspace.plan().is_ok());
    workspace
        .traces
        .iter_mut()
        .find(|trace| trace.id == reflection)
        .unwrap()
        .settings
        .visible = true;
    assert!(workspace.plan().is_err());
    for list in [
        Vec::new(),
        vec![1_000_000; 2],
        vec![1_000_000; 1002],
        vec![2_000_000, 1_000_000, 3_000_000],
    ] {
        workspace.frequencies_hz = list;
        assert!(workspace.plan().is_err());
    }
}

#[test]
fn all_equal_frequency_views_are_finite_without_changing_requested_points() {
    for frequency in [0, 1_000_000] {
        for log_x in [false, true] {
            let mut workspace = workspace(TraceDisplay::Spec, vec![frequency; 3]);
            let range = workspace.range;
            workspace.log_x = log_x;
            workspace.reset_frequency_view();
            assert!(workspace.x_view.x_min.is_finite());
            assert!(workspace.x_view.x_max.is_finite());
            assert!(workspace.x_view.x_min < workspace.x_view.x_max);
            if log_x {
                assert!(workspace.x_view.x_min > 0.0);
            }
            if frequency > 0 || !log_x {
                assert!(workspace.x_view.x_min <= frequency as f64);
                assert!(workspace.x_view.x_max >= frequency as f64);
            }
            assert_eq!(workspace.frequencies_hz, vec![frequency; 3]);
            assert_eq!(workspace.range, range);
            assert!(workspace.plan().is_ok());
        }
    }
}

#[test]
fn log_list_view_keeps_the_first_positive_target_and_full_stop() {
    let mut workspace = workspace(TraceDisplay::Spec, vec![0, 1, 10_000]);
    let range = workspace.range;
    workspace.log_x = true;
    workspace.reset_frequency_view();
    assert_eq!(workspace.x_view.x_min, 1.0);
    assert_eq!(workspace.x_view.x_max, 10_000.0);
    assert_eq!(workspace.range, range);
    assert_eq!(workspace.frequencies_hz, [0, 1, 10_000]);
}

#[test]
fn old_completed_data_cannot_hide_a_new_lists_lowest_positive_target() {
    let mut workspace = workspace(TraceDisplay::Spec, vec![0, 1, 10_000]);
    let mut settings = crate::acquisition::tests::spec();
    settings.start_hz = 1_000;
    settings.stop_hz = 10_000;
    workspace.traces[0].completed = Some(std::sync::Arc::new(crate::acquisition::CompletedSweep {
        segments: None,
        data: kcsdi_core::data::SweepData {
            mode: kcsdi_core::protocol::StreamMode::Spec,
            format: String::new(),
            points: [1_000.0, 5_000.0, 10_000.0]
                .into_iter()
                .map(|freq_hz| kcsdi_core::data::SweepPoint {
                    freq_hz,
                    values: vec![-20.0],
                })
                .collect(),
        },
        settings: AcquisitionSettings::Spec(settings),
        session_id: 1,
        completed_at: std::time::SystemTime::UNIX_EPOCH,
    }));
    workspace.log_x = true;
    workspace.reset_frequency_view();
    assert_eq!(workspace.x_view.x_min, 1.0);
    assert_eq!(workspace.x_view.x_max, 10_000.0);
    assert!(workspace.traces[0].completed.is_some());
}

#[test]
fn marker_center_refuses_lists_without_mutating_any_frequency_definition() {
    let mut workspace = workspace(TraceDisplay::S11(S11Display::Impedance), vec![1_000_000; 3]);
    workspace.reset_frequency_view();
    let range = workspace.range;
    let view = workspace.x_view;
    let list = workspace.frequencies_hz.clone();
    for frequency in [1_000_000.0, 2_000_000.0, f64::NAN] {
        assert!(workspace.center_on_marker(frequency).is_err());
        assert_eq!(workspace.range, range);
        assert_eq!(workspace.x_view, view);
        assert_eq!(workspace.frequencies_hz, list);
    }
}

#[test]
fn list_reconciliation_restarts_on_same_count_edits_but_not_selection_or_style() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut state = AppState {
        workspace: workspace(
            TraceDisplay::S11(S11Display::Impedance),
            vec![1_000_000, 1_000_000, 2_000_000],
        ),
        connection: ConnectionState::Connected,
        cmd_tx: Some(sender),
        ..Default::default()
    };
    state.send(WorkerCommand::RunWorkspace(state.workspace.plan().unwrap()));
    let original = receiver.try_recv().unwrap();
    state.workspace.traces[0].settings.display = TraceDisplay::S11(S11Display::Smith);
    state.workspace.traces[0].settings.line_width = 2.0;
    state.workspace.selected = None;
    state.reconcile_plan();
    assert_eq!(state.request_id, original.request_id);
    assert!(!original.cancel.is_cancelled());
    assert!(receiver.try_recv().is_err());
    state.workspace.frequencies_hz[1] = 1_500_000;
    state.reconcile_plan();
    let replacement = receiver.try_recv().unwrap();
    assert_eq!(replacement.request_id, original.request_id + 1);
    assert!(original.cancel.is_cancelled());
    assert_eq!(replacement.session_id, original.session_id);
    assert!(matches!(
        replacement.command,
        WorkerCommand::RunWorkspace(_)
    ));
}

#[test]
fn list_and_inactive_range_round_trip_without_resuming_acquisition() {
    for list_mode in [false, true] {
        let mut original = AppState {
            workspace: workspace(
                TraceDisplay::S21(S21Display::Delay),
                vec![1_000_000, 1_000_000, 1_700_003],
            ),
            ..Default::default()
        };
        original.workspace.range = SweepRange::new(200_000_000.0, 400_000_000.0, 101);
        original.workspace.list_mode = list_mode;
        original.workspace.reset_frequency_view();
        original.send(WorkerCommand::RunWorkspace(
            original.workspace.plan().unwrap(),
        ));
        let config = AppConfig::from_state(&original);
        let encoded = toml::to_string(&config).unwrap();
        let loaded: AppConfig = toml::from_str(&encoded).unwrap();
        let mut restored = AppState::default();
        loaded.apply_to(&mut restored);
        assert_eq!(restored.workspace.list_mode, list_mode);
        assert_eq!(
            restored.workspace.frequencies_hz,
            original.workspace.frequencies_hz
        );
        assert_eq!(restored.workspace.range, original.workspace.range);
        assert_eq!(
            restored.workspace.plan().unwrap(),
            original.workspace.plan().unwrap()
        );
        assert_eq!(
            restored.workspace.x_view.x_min,
            original.workspace.x_view.x_min
        );
        assert_eq!(
            restored.workspace.x_view.x_max,
            original.workspace.x_view.x_max
        );
        assert_eq!(restored.sweep, SweepState::Idle);
        assert!(restored.active_plan.is_none());
        assert_eq!(restored.connection, ConnectionState::Disconnected);
        assert!(
            restored
                .workspace
                .traces
                .iter()
                .all(|trace| trace.completed.is_none() && trace.preview.is_none())
        );
    }
}

#[test]
fn restored_list_without_a_valid_saved_view_uses_the_list_bounds() {
    for log_x in [false, true] {
        let mut source = AppState {
            workspace: workspace(TraceDisplay::Spec, vec![0, 1, 10_000]),
            ..Default::default()
        };
        source.workspace.log_x = log_x;
        let mut config = AppConfig::from_state(&source);
        config.workspace.x_view = None;
        let mut restored = AppState::default();
        config.apply_to(&mut restored);
        assert_eq!(
            restored.workspace.x_view.x_min,
            if log_x { 1.0 } else { 0.0 }
        );
        assert_eq!(restored.workspace.x_view.x_max, 10_000.0);
        assert!(restored.workspace.plan().is_ok());
    }
}
