// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use super::*;
use crate::state::{S11Display, S21Display};
use crate::workspace::{SweepRange, TraceDisplay, TraceSettings, Workspace};
use kcsdi_core::commands::{Cal, Lo};
use kcsdi_core::model::Rbw;

fn reflection_state() -> AppState {
    let mut state = AppState {
        connection: ConnectionState::Connected,
        workspace: Workspace::empty(SweepRange::new(1_000_000.0, 2_000_001.0, 201)),
        ..Default::default()
    };
    state
        .workspace
        .add_trace(TraceSettings {
            display: TraceDisplay::S11(S11Display::Impedance),
            rbw: Rbw::R3k,
            cal: Cal::CalSys,
            ..Default::default()
        })
        .unwrap();
    state
}

#[test]
fn user_definition_freezes_matching_selected_trace_and_rejects_lists() {
    let mut state = reflection_state();
    let frozen = freeze(&state, CalibrationKind::S11User).unwrap();
    assert_eq!(frozen.trace_id, state.workspace.selected);
    assert_eq!(
        frozen.params.user().unwrap().endpoints().unwrap(),
        (1_000_000, 2_000_001)
    );
    assert_eq!(frozen.params.user().unwrap().points, 201);
    assert_eq!(frozen.params.user().unwrap().rbw, Rbw::R3k);
    assert!(freeze(&state, CalibrationKind::S21User).is_err());
    state.workspace.range = SweepRange::new(4_000_000.0, 8_000_000.0, 501);
    state.workspace.selected_mut().unwrap().settings.rbw = Rbw::R30k;
    assert_eq!(
        frozen.params.user().unwrap().endpoints().unwrap(),
        (1_000_000, 2_000_001)
    );
    assert_eq!(frozen.params.user().unwrap().rbw, Rbw::R3k);
    state.workspace.sweep_mode = crate::workspace::SweepMode::List;
    assert!(freeze(&state, CalibrationKind::S11User).is_err());
    assert!(freeze(&state, CalibrationKind::S11System).is_ok());
    state.workspace.sweep_mode = crate::workspace::SweepMode::Segments;
    assert!(freeze(&state, CalibrationKind::S11User).is_err());
    assert!(freeze(&state, CalibrationKind::S11System).is_ok());
    state.workspace.sweep_mode = crate::workspace::SweepMode::Range;
    let trace = state.workspace.selected_mut().unwrap();
    trace.settings.display = TraceDisplay::S21(S21Display::Delay);
    trace.settings.lo = Lo::LowLo;
    let first = freeze(&state, CalibrationKind::S21User).unwrap();
    let trace = state.workspace.selected_mut().unwrap();
    trace.settings.lo = Lo::HighLo;
    trace.settings.cal = Cal::CalOff;
    trace.settings.display = TraceDisplay::S21(S21Display::Loss);
    assert_eq!(first, freeze(&state, CalibrationKind::S21User).unwrap());
    state.function = crate::source_panel::InstrumentFunction::RfSource;
    assert!(freeze(&state, CalibrationKind::S21User).is_err());
    assert!(freeze(&state, CalibrationKind::S21System).is_ok());
}

fn click(ctx: &egui::Context, state: &mut AppState, caption: &str, time: &mut f64) {
    let mut pointer = egui::Pos2::ZERO;
    for frame in 0..6 {
        *time += 1.0 / 30.0;
        let events = if frame == 3 || frame == 4 {
            vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::PointerButton {
                    pos: pointer,
                    button: egui::PointerButton::Primary,
                    pressed: frame == 3,
                    modifiers: Default::default(),
                },
            ]
        } else {
            Vec::new()
        };
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                time: Some(*time),
                events,
                ..Default::default()
            },
            |ui| show(ui.ctx(), state),
        );
        let shapes = std::mem::take(&mut output.shapes);
        output.drop_without_applying_deltas();
        if frame == 2 {
            pointer = shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.job.text == caption
                    {
                        Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                    } else {
                        None
                    }
                })
                .expect("calibration control visible");
        }
    }
}

#[test]
fn consent_and_actual_next_clicks_never_advance_without_device_packets() {
    for language in Language::ALL {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut state = reflection_state();
        state.language = language;
        let (tx, rx) = std::sync::mpsc::channel();
        state.cmd_tx = Some(tx);
        state.calibration.open();
        let mut time = 0.0;
        click(
            &ctx,
            &mut state,
            language.text(Text::CalibrationS11User),
            &mut time,
        );
        click(&ctx, &mut state, language.text(Text::Next), &mut time);
        let frozen = state.calibration.frozen.clone().unwrap();
        assert!(rx.try_recv().is_err());
        click(
            &ctx,
            &mut state,
            language.text(Text::CalibrationStart),
            &mut time,
        );
        assert!(rx.try_recv().is_err());
        click(
            &ctx,
            &mut state,
            language.text(Text::CalibrationConsent),
            &mut time,
        );
        click(
            &ctx,
            &mut state,
            language.text(Text::CalibrationStart),
            &mut time,
        );
        let start = rx.try_recv().unwrap();
        assert!(
            matches!(start.command, WorkerCommand::StartCalibration(params) if params == frozen.params)
        );
        assert_eq!(state.calibration.pending, Some(Pending::Start));
        assert_eq!(state.calibration.report.phase, CalibrationPhase::NotStarted);
        state.calibration.accept(CalibrationReport {
            kind: Some(CalibrationKind::S11User),
            phase: CalibrationPhase::Prompt(CalibrationPrompt::Short),
        });
        click(&ctx, &mut state, language.text(Text::Next), &mut time);
        let advance = rx.try_recv().unwrap();
        assert!(matches!(
            advance.command,
            WorkerCommand::AdvanceCalibration(CalibrationPrompt::Short)
        ));
        assert_eq!(state.calibration.pending, Some(Pending::Advance));
        assert_eq!(
            state.calibration.report.phase,
            CalibrationPhase::Prompt(CalibrationPrompt::Short)
        );
        click(&ctx, &mut state, language.text(Text::Next), &mut time);
        assert!(rx.try_recv().is_err());
        assert!(advance.request_id > start.request_id);
        click(&ctx, &mut state, language.text(Text::Cancel), &mut time);
        assert!(matches!(
            rx.try_recv().unwrap().command,
            WorkerCommand::CancelCalibration
        ));
        assert!(state.calibration.open);
        assert_eq!(state.calibration.pending, Some(Pending::Cancel));
        state.calibration.accept(CalibrationReport {
            kind: Some(CalibrationKind::S11User),
            phase: CalibrationPhase::Cancelled,
        });
        assert!(!state.calibration.busy());
        click(&ctx, &mut state, language.text(Text::Close), &mut time);
        assert!(!state.calibration.open);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn escape_cancels_active_step_and_retains_unknown_until_explicit_new_calibration() {
    let ctx = egui::Context::default();
    crate::theme::setup(&ctx);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut state = AppState {
        connection: ConnectionState::Connected,
        cmd_tx: Some(tx),
        ..Default::default()
    };
    state.send(WorkerCommand::StartCalibration(
        CalibrationParams::S21System,
    ));
    rx.try_recv().unwrap();
    state.calibration.accept(CalibrationReport {
        kind: Some(CalibrationKind::S21System),
        phase: CalibrationPhase::Measuring,
    });
    for frame in 0..3 {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                events: if frame == 2 {
                    vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Default::default(),
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            |ui| show(ui.ctx(), &mut state),
        )
        .drop_without_applying_deltas();
    }
    assert!(matches!(
        rx.try_recv().unwrap().command,
        WorkerCommand::CancelCalibration
    ));
    assert!(state.calibration.open && state.calibration.busy());
    state.calibration.accept(CalibrationReport {
        kind: None,
        phase: CalibrationPhase::Unknown,
    });
    assert_eq!(
        state.calibration.report.kind,
        Some(CalibrationKind::S21System)
    );
    assert!(!state.calibration.busy());
    state.calibration.lost();
    assert_eq!(state.calibration.report.phase, CalibrationPhase::Unknown);
    state.send(WorkerCommand::AdvanceCalibration(
        CalibrationPrompt::Through,
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn only_packets_complete_steps_and_reopening_clears_write_consent() {
    let mut ui = CalibrationUi::default();
    ui.begin(CalibrationParams::S11System);
    assert_eq!(ui.completed_standards, 0);
    assert_eq!(ui.current_prompt, None);
    for (phase, completed, current) in [
        (CalibrationPhase::WarmingUp, 0, None),
        (
            CalibrationPhase::Prompt(CalibrationPrompt::Short),
            0,
            Some(CalibrationPrompt::Short),
        ),
        (
            CalibrationPhase::Measuring,
            0,
            Some(CalibrationPrompt::Short),
        ),
        (
            CalibrationPhase::Prompt(CalibrationPrompt::Open),
            1,
            Some(CalibrationPrompt::Open),
        ),
        (
            CalibrationPhase::Prompt(CalibrationPrompt::Load),
            2,
            Some(CalibrationPrompt::Load),
        ),
        (
            CalibrationPhase::Processing,
            2,
            Some(CalibrationPrompt::Load),
        ),
        (CalibrationPhase::Saving, 2, Some(CalibrationPrompt::Load)),
        (CalibrationPhase::Completed, 3, None),
    ] {
        ui.accept(CalibrationReport {
            kind: Some(CalibrationKind::S11System),
            phase,
        });
        assert_eq!(ui.completed_standards, completed);
        assert_eq!(ui.current_prompt, current);
    }
    let mut review = CalibrationUi {
        frozen: Some(FrozenCalibration {
            params: CalibrationParams::S11System,
            trace_id: None,
        }),
        consent: true,
        ..Default::default()
    };
    review.open();
    assert!(!review.consent);
    assert_eq!(review.report.phase, CalibrationPhase::NotStarted);
}
