// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use super::*;

fn definitions() -> Vec<Segment> {
    vec![
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
    ]
}

fn receivers() -> Vec<(TraceId, PointSettings)> {
    vec![(
        TraceId(1),
        crate::segmented::tests::example_snapshot()
            .settings
            .receiver(),
    )]
}

#[test]
fn segment_inputs_accept_si_units_but_reject_fractional_hz_and_invalid_numbers() {
    for (text, expected) in [
        ("5 kHz", 5000),
        ("0.05 MHz", 50_000),
        ("1 GHz", 1_000_000_000),
        ("5e3", 5000),
        ("0 Hz", 0),
        ("0.001 kHz", 1),
        ("8.001 kHz", 8001),
        ("1.000000001 GHz", 1_000_000_001),
        ("18446744073709551615 Hz", u64::MAX),
    ] {
        assert_eq!(whole_hz(text), Some(expected));
    }
    for text in [
        "",
        "-1",
        "NaN",
        "inf",
        "0.5 Hz",
        "0.0000015 MHz",
        "18446744073709551616",
        "1 ohm",
        "8.0010000000000001 kHz",
        "1.0000000001 Hz",
    ] {
        assert_eq!(whole_hz(text), None, "{text}");
    }
}

#[test]
fn opening_whole_hz_segments_never_changes_their_values() {
    for value in
        (5000..100_000)
            .step_by(7)
            .chain([1_000_001, 50_000_001, 1_000_000_001, 7_000_000_000])
    {
        assert_eq!(
            whole_hz(&format_axis_value(value as f64, "Hz")),
            Some(value)
        );
    }
}

#[test]
fn drafts_are_independent_and_adding_a_row_keeps_an_unset_stop() {
    let source = definitions();
    let mut editor = SegmentEditor::default();
    editor.open(&source, &SweepRange::default());
    editor.add_next();
    assert_eq!(editor.rows[2].fields, ["1 GHz", "", "1 MHz"]);
    assert!(editor.definitions(Language::English).is_err());
    editor.rows.remove(1);
    editor.rows[1].fields[1] = "2 GHz".into();
    let rows = editor.definitions(Language::English).unwrap();
    let error = validate(
        &rows,
        &receivers(),
        DEVICE_MODEL.capabilities().s11.range,
        Language::English,
    )
    .unwrap_err();
    assert_eq!(error.row, Some(1));
    assert_eq!(source, definitions());
    editor.cancel();
    editor.open(&source, &SweepRange::default());
    assert_eq!(editor.definitions(Language::English).unwrap(), source);
}

#[test]
fn table_validation_covers_limits_receivers_and_both_languages() {
    for language in Language::ALL {
        let rows = definitions();
        let allowed = DEVICE_MODEL.capabilities().s11.range;
        assert_eq!(
            validate(&rows, &receivers(), allowed, language).unwrap(),
            1952
        );
        let mut too_dense = rows.clone();
        too_dense[0].max_step_hz = 1000;
        let error = validate(&too_dense, &receivers(), allowed, language).unwrap_err();
        assert_eq!(error.row, Some(0));
        assert!(error.text.contains("49996"));
        assert!(error.text.contains(language.text(Text::SegmentPointsHelp)));
        let mut below_range = rows;
        below_range[0].start_hz = 0;
        assert!(
            validate(&below_range, &receivers(), allowed, language)
                .unwrap_err()
                .text
                .contains(language.text(Text::SegmentFrequencyLimits))
        );
        assert!(validate(&definitions(), &[], allowed, language).is_err());
    }
}

fn click_apply(
    editor: &mut SegmentEditor,
    language: Language,
    can_apply: bool,
) -> Option<Vec<Segment>> {
    let ctx = egui::Context::default();
    crate::theme::setup(&ctx);
    let mut pointer = egui::Pos2::ZERO;
    let mut applied = None;
    for frame in 0..6 {
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
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                time: Some(f64::from(frame) / 30.0),
                events,
                ..Default::default()
            },
            |ui| {
                if let Some(rows) = editor.show(
                    ui.ctx(),
                    language,
                    DEVICE_MODEL.capabilities().s11.range,
                    &receivers(),
                    can_apply,
                ) {
                    applied = Some(rows);
                }
            },
        );
        for shape in &output.shapes {
            if frame >= 2
                && let egui::Shape::Text(text) = &shape.shape
                && ["5 kHz", "50 MHz", "50 kHz", "1 GHz", "1 MHz"].contains(&text.galley.text())
            {
                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                assert!(
                    shape.clip_rect.contains_rect(bounds),
                    "input clipped: {}",
                    text.galley.text()
                );
            }
            if let egui::Shape::Text(text) = &shape.shape
                && text.galley.text() == language.text(Text::Apply)
            {
                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                assert!(shape.clip_rect.contains_rect(bounds));
                pointer = bounds.center();
            }
        }
        output.drop_without_applying_deltas();
    }
    applied
}

#[test]
fn apply_is_transactional_and_disabled_during_acquisition_in_both_languages() {
    for language in Language::ALL {
        let mut editor = SegmentEditor::default();
        editor.open(&definitions(), &SweepRange::default());
        assert!(click_apply(&mut editor, language, false).is_none());
        assert!(editor.open);
        assert_eq!(
            click_apply(&mut editor, language, true),
            Some(definitions())
        );
        assert!(!editor.open);
        editor.open(&definitions(), &SweepRange::default());
        editor.rows[0].fields[2] = "0".into();
        assert!(click_apply(&mut editor, language, true).is_none());
        assert!(editor.open);
    }
}
