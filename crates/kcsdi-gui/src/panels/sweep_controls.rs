// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared sweep inputs and preflight feedback for the KC901V panels.

use kcsdi_core::model::FreqRange;

use crate::i18n::{Language, Text};
use crate::state::DEVICE_MODEL;
use crate::theme::PRIMARY;

pub(super) struct SweepFields<'a> {
    pub start: &'a mut f64,
    pub stop: &'a mut f64,
    pub center: &'a mut f64,
    pub span: &'a mut f64,
    pub points: &'a mut u32,
}

pub(super) enum SweepEdit {
    None,
    StartStop,
    CenterSpan,
}

impl SweepFields<'_> {
    pub fn show(self, ui: &mut egui::Ui, range: FreqRange, language: Language) -> SweepEdit {
        let caps = DEVICE_MODEL.capabilities();
        let mut start_stop_edited = false;
        let mut center_span_edited = false;
        egui::Grid::new("sweep_grid")
            .num_columns(2)
            .spacing([8.0, 8.0])
            .show(ui, |ui| {
                ui.label(language.text(Text::Start));
                start_stop_edited |= frequency_field(ui, self.start);
                ui.end_row();
                ui.label(language.text(Text::Stop));
                start_stop_edited |= frequency_field(ui, self.stop);
                ui.end_row();
                ui.label(language.text(Text::Center));
                center_span_edited |= frequency_field(ui, self.center);
                ui.end_row();
                ui.label(language.text(Text::Span));
                center_span_edited |= frequency_field(ui, self.span);
                ui.end_row();
                ui.label(language.text(Text::Points));
                ui.add(
                    egui::DragValue::new(self.points)
                        .range(caps.points_min..=caps.points_max)
                        .clamp_existing_to_range(false),
                )
                .on_hover_text(language.text(Text::PointsHelp));
                ui.end_row();
            });
        ui.small(format!(
            "{}: {}..{} MHz",
            language.text(Text::FrequencyRange),
            range.min_hz as f64 / 1e6,
            range.max_hz as f64 / 1e6
        ));
        ui.small(format!(
            "{}: {} kHz",
            language.text(Text::MinimumSpan),
            range.min_span_hz as f64 / 1e3
        ));
        if start_stop_edited {
            SweepEdit::StartStop
        } else if center_span_edited {
            SweepEdit::CenterSpan
        } else {
            SweepEdit::None
        }
    }
}

/// Keep the original input visible on failure, including values loaded
/// from older config files. Preflight blocks RUN instead of changing it.
fn frequency_field(ui: &mut egui::Ui, hz: &mut f64) -> bool {
    let mut mhz = *hz / 1e6;
    let changed = ui
        .add(
            egui::DragValue::new(&mut mhz)
                .speed(0.1)
                .suffix(" MHz")
                .max_decimals(6),
        )
        .changed();
    if changed {
        *hz = mhz * 1e6;
    }
    changed
}

/// A disabled RUN button and inline diagnostics when preflight fails.
/// The returned parameters are exactly the ones that were validated.
pub(super) fn run_button<P>(
    ui: &mut egui::Ui,
    params: kcsdi_core::Result<P>,
    language: Language,
) -> Option<P> {
    if let Err(error) = &params {
        ui.colored_label(
            egui::Color32::from_rgb(0xff, 0x90, 0x80),
            format!("{}: {error}", language.text(Text::Error)),
        );
    }
    let clicked = ui
        .add_enabled_ui(params.is_ok(), |ui| {
            ui.add_sized(
                [ui.available_width(), 32.0],
                egui::Button::new(egui::RichText::new(language.text(Text::Run)).strong())
                    .fill(PRIMARY),
            )
            .clicked()
        })
        .inner;
    if clicked { params.ok() } else { None }
}

pub(super) fn group_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(text).strong().small());
    });
    ui.separator();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_parameters_disable_run_without_silently_replacing_input() {
        for valid in [false, true] {
            let ctx = egui::Context::default();
            let mut clicked = false;
            let mut draw = |events| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(500.0, 400.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let params = if valid {
                            Ok(201)
                        } else {
                            Err(kcsdi_core::Error::InvalidParameter("S11 start 0 Hz".into()))
                        };
                        clicked |= run_button(ui, params, Language::English).is_some();
                    },
                )
            };
            let output = draw(vec![]);
            let position = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "RUN" => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap();
            output.drop_without_applying_deltas();
            draw(vec![egui::Event::PointerMoved(position)]).drop_without_applying_deltas();
            for pressed in [true, false] {
                draw(vec![egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }])
                .drop_without_applying_deltas();
            }
            assert_eq!(clicked, valid);
        }
    }

    #[test]
    fn drawing_fields_preserves_out_of_range_legacy_settings() {
        let ctx = egui::Context::default();
        let mut start = -1.0;
        let mut stop = 7_000_000_001.0;
        let mut center = 0.0;
        let mut span = 0.0;
        let mut points = 10001;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let edit = SweepFields {
                start: &mut start,
                stop: &mut stop,
                center: &mut center,
                span: &mut span,
                points: &mut points,
            }
            .show(ui, DEVICE_MODEL.capabilities().s11.range, Language::English);
            assert!(matches!(edit, SweepEdit::None));
        });
        output.drop_without_applying_deltas();
        assert_eq!(start, -1.0);
        assert_eq!(stop, 7_000_000_001.0);
        assert_eq!(points, 10001);
    }
}
