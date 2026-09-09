// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared sweep inputs and preflight feedback for the KC901V panels.

use kcsdi_core::model::FreqRange;

use crate::i18n::{Language, Text};
use crate::state::DEVICE_MODEL;
use crate::theme::PRIMARY;
use crate::widgets::plot::PlotView;

const FIELD_HEIGHT: f32 = 46.0;
pub(super) const BUTTON_HEIGHT: f32 = 36.0;

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

impl SweepEdit {
    /// Explicit sweep edits replace X zoom without disturbing manual Y scale.
    pub(super) fn sync_view(&self, view: &mut PlotView, range: FreqRange, start: f64, stop: f64) {
        if matches!(self, Self::None) {
            return;
        }
        let (Ok(start), Ok(stop)) = (
            kcsdi_core::validation::frequency_hz(start, "start"),
            kcsdi_core::validation::frequency_hz(stop, "stop"),
        ) else {
            return;
        };
        if range.contains_sweep(start, stop) {
            view.x_min = start as f64;
            view.x_max = stop as f64;
        }
    }
}

impl SweepFields<'_> {
    pub fn show(
        self,
        ui: &mut egui::Ui,
        range: FreqRange,
        language: Language,
        mode: &'static str,
    ) -> SweepEdit {
        let caps = DEVICE_MODEL.capabilities();
        let mut start_stop_edited = false;
        let mut center_span_edited = false;
        let limits = format!(
            "{} - {}",
            compact_frequency(range.min_hz as f64),
            compact_frequency(range.max_hz as f64)
        );
        for (key, id, value) in [
            (Text::Start, "start", &mut *self.start),
            (Text::Stop, "stop", &mut *self.stop),
        ] {
            field_label(ui, &format!("{} ({limits})", language.text(key)));
            start_stop_edited |= frequency_field(ui, value, (mode, id));
        }
        field_label(ui, &format!("{} ({limits})", language.text(Text::Center)));
        center_span_edited |= frequency_field(ui, self.center, (mode, "center"));
        field_label(
            ui,
            &format!(
                "{} ({}: {})",
                language.text(Text::Span),
                language.text(Text::MinimumSpan),
                compact_frequency(range.min_span_hz as f64)
            ),
        );
        center_span_edited |= frequency_field(ui, self.span, (mode, "span"));
        field_label(
            ui,
            &format!(
                "{} ({} - {})",
                language.text(Text::Points),
                caps.points_min,
                caps.points_max
            ),
        );
        numeric_field(ui, ui.available_width(), |ui| {
            ui.add_sized(
                [ui.available_width(), FIELD_HEIGHT - 16.0],
                egui::DragValue::new(self.points)
                    .range(caps.points_min..=caps.points_max)
                    .clamp_existing_to_range(false),
            )
            .on_hover_text(language.text(Text::PointsHelp));
        });
        let span = if start_stop_edited {
            *self.stop - *self.start
        } else {
            *self.span
        };
        step_field(ui, mode, span, self.points, language);
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
fn frequency_field(ui: &mut egui::Ui, hz: &mut f64, key: (&str, &str)) -> bool {
    let id = egui::Id::new(("frequency_unit", key));
    let mut unit = ui
        .ctx()
        .data(|data| data.get_temp::<FrequencyUnit>(id))
        .unwrap_or_else(|| FrequencyUnit::for_hz(*hz));
    let multiplier = unit.multiplier();
    let mut value = *hz / unit.multiplier();
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 72.0).max(40.0);
        numeric_field(ui, width, |ui| {
            changed = ui
                .add_sized(
                    [ui.available_width(), FIELD_HEIGHT - 16.0],
                    egui::DragValue::new(&mut value)
                        .speed(0.01)
                        .max_decimals(unit.decimals())
                        .custom_formatter(move |value, _| frequency_readout(value, unit)),
                )
                .changed();
        });
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = FIELD_HEIGHT;
            egui::ComboBox::from_id_salt(id)
                .width(64.0)
                .selected_text(unit.label())
                .show_ui(ui, |ui| {
                    for choice in FrequencyUnit::ALL {
                        ui.selectable_value(&mut unit, choice, choice.label());
                    }
                });
        });
    });
    if changed {
        // Changing the unit selector alone never changes the frequency.
        *hz = value * multiplier;
    }
    ui.ctx().data_mut(|data| data.insert_temp(id, unit));
    changed
}

#[derive(Clone, Copy)]
struct StepDraft {
    span: f64,
    points: u32,
    hz: f64,
    invalid: bool,
}

fn step_id(mode: &str) -> egui::Id {
    egui::Id::new(("sweep_step", mode))
}

pub(super) fn invalid_step(ctx: &egui::Context, mode: &str) -> bool {
    ctx.data(|data| data.get_temp::<StepDraft>(step_id(mode)))
        .is_some_and(|draft| draft.invalid)
}

fn points_for_step(span: f64, step: f64) -> Option<u32> {
    let caps = DEVICE_MODEL.capabilities();
    if !span.is_finite() || span <= 0.0 || !step.is_finite() || step <= 0.0 {
        return None;
    }
    let points = (span / step).round() + 1.0;
    (points >= f64::from(caps.points_min) && points <= f64::from(caps.points_max))
        .then_some(points as u32)
}

fn step_field(ui: &mut egui::Ui, mode: &str, span: f64, points: &mut u32, language: Language) {
    let id = step_id(mode);
    let mut draft = ui
        .ctx()
        .data(|data| data.get_temp::<StepDraft>(id))
        .filter(|draft| draft.span == span && draft.points == *points)
        .unwrap_or(StepDraft {
            span,
            points: *points,
            hz: span / f64::from(points.saturating_sub(1).max(1)),
            invalid: false,
        });
    field_label(ui, language.text(Text::Step)).on_hover_text(language.text(Text::StepHelp));
    if frequency_field(ui, &mut draft.hz, (mode, "step")) {
        if let Some(count) = points_for_step(span, draft.hz) {
            *points = count;
            draft.points = count;
            draft.hz = span / f64::from(count - 1);
            draft.invalid = false;
        } else {
            draft.invalid = true;
        }
    }
    if draft.invalid {
        ui.colored_label(
            ui.visuals().error_fg_color,
            language.text(Text::InvalidStep),
        );
    }
    ui.ctx().data_mut(|data| data.insert_temp(id, draft));
}

#[derive(Clone, Copy, PartialEq)]
enum FrequencyUnit {
    Hz,
    Kilohertz,
    Megahertz,
    Gigahertz,
}

impl FrequencyUnit {
    const ALL: [Self; 4] = [Self::Hz, Self::Kilohertz, Self::Megahertz, Self::Gigahertz];

    fn for_hz(hz: f64) -> Self {
        if hz.abs() >= 1e9 {
            Self::Gigahertz
        } else if hz.abs() >= 1e6 {
            Self::Megahertz
        } else if hz.abs() >= 1e3 {
            Self::Kilohertz
        } else {
            Self::Hz
        }
    }

    fn multiplier(self) -> f64 {
        match self {
            Self::Hz => 1.0,
            Self::Kilohertz => 1e3,
            Self::Megahertz => 1e6,
            Self::Gigahertz => 1e9,
        }
    }

    fn decimals(self) -> usize {
        match self {
            Self::Hz => 0,
            Self::Kilohertz => 3,
            Self::Megahertz => 6,
            Self::Gigahertz => 9,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hz => "Hz",
            Self::Kilohertz => "kHz",
            Self::Megahertz => "MHz",
            Self::Gigahertz => "GHz",
        }
    }
}

fn compact_frequency(hz: f64) -> String {
    let unit = FrequencyUnit::for_hz(hz);
    format!("{} {}", hz / unit.multiplier(), unit.label())
}

fn frequency_readout(value: f64, unit: FrequencyUnit) -> String {
    let text = format!("{value:.precision$}", precision = unit.decimals());
    if unit.decimals() == 0 {
        text
    } else {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

pub(super) fn field_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(ui.visuals().weak_text_color()),
    )
}

fn numeric_field(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let frame = egui::Frame::new()
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(4)
        .inner_margin(8);
    let inner_width = (width - frame.total_margin().sum().x).max(24.0);
    frame.show(ui, |ui| {
        ui.set_width(inner_width);
        ui.style_mut().override_font_id = Some(egui::FontId::monospace(20.0));
        ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::NONE;
        add(ui);
    });
}

pub(super) fn scale_fields(
    ui: &mut egui::Ui,
    view: &mut PlotView,
    language: Language,
    unit: &str,
    top_reference: bool,
) -> bool {
    group_heading(ui, language.text(Text::Scale));
    let mut reference = if top_reference {
        view.y_max
    } else {
        (view.y_min + view.y_max) / 2.0
    };
    let mut divisions = view.y_divisions;
    let mut per_division = (view.y_max - view.y_min) / divisions.max(1) as f64;
    let seconds = unit == "s";
    let drag_step = if seconds {
        (per_division.abs() * 0.02).max(1e-15)
    } else {
        0.1
    };
    let mut changed = false;
    let label_width = [Text::Reference, Text::Divisions, Text::PerDivision]
        .into_iter()
        .map(|key| {
            ui.fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap(
                        language.text(key).to_owned(),
                        egui::TextStyle::Small.resolve(ui.style()),
                        ui.visuals().text_color(),
                    )
                    .size()
                    .x
            })
        })
        .fold(0.0_f32, f32::max);
    let width = (ui.available_width() - label_width - ui.spacing().item_spacing.x).max(40.0);
    egui::Grid::new("scale_fields")
        .num_columns(2)
        .min_col_width(0.0)
        .show(ui, |ui| {
            let reference_help = if unit.is_empty() {
                language.text(Text::Reference).to_string()
            } else {
                format!("{} ({unit})", language.text(Text::Reference))
            };
            field_label(ui, language.text(Text::Reference)).on_hover_text(reference_help);
            let mut reference_edit =
                egui::DragValue::new(&mut reference).speed(if seconds { drag_step } else { 1.0 });
            if seconds {
                reference_edit = reference_edit.custom_formatter(|value, _| format!("{value:.3e}"));
            }
            changed |= ui.add_sized([width, 36.0], reference_edit).changed();
            ui.end_row();
            field_label(ui, language.text(Text::Divisions));
            changed |= ui
                .add_sized(
                    [width, 36.0],
                    egui::DragValue::new(&mut divisions).range(2..=30),
                )
                .changed();
            ui.end_row();
            field_label(ui, language.text(Text::PerDivision)).on_hover_text(if unit.is_empty() {
                language.text(Text::PerDivision).to_owned()
            } else {
                format!("{} ({unit})", language.text(Text::PerDivision))
            });
            let mut division_edit = egui::DragValue::new(&mut per_division)
                .range(if seconds {
                    1e-15..=1e3
                } else {
                    0.000_001..=1e12
                })
                .clamp_existing_to_range(false)
                .speed(drag_step);
            if seconds {
                division_edit = division_edit.custom_formatter(|value, _| format!("{value:.3e}"));
            }
            changed |= ui.add_sized([width, 36.0], division_edit).changed();
            ui.end_row();
        });
    let span = divisions as f64 * per_division;
    if changed && reference.is_finite() && span.is_finite() && span > 0.0 {
        view.y_max = if top_reference {
            reference
        } else {
            reference + span / 2.0
        };
        view.y_min = view.y_max - span;
        view.y_divisions = divisions;
        true
    } else {
        false
    }
}

pub(super) fn choice_button(ui: &mut egui::Ui, text: &str, selected: bool, width: f32) -> bool {
    ui.add_sized(
        [width, BUTTON_HEIGHT],
        egui::Button::new(egui::RichText::new(text).small().strong()).selected(selected),
    )
    .clicked()
}

/// A disabled RUN button and inline diagnostics when preflight fails.
/// The returned parameters are exactly the ones that were validated.
pub(super) fn run_button<P>(
    ui: &mut egui::Ui,
    params: kcsdi_core::Result<P>,
    language: Language,
    caption: Text,
) -> Option<P> {
    let clicked = ui
        .add_enabled_ui(params.is_ok(), |ui| {
            ui.add_sized(
                [ui.available_width(), BUTTON_HEIGHT],
                egui::Button::new(egui::RichText::new(language.text(caption)).strong())
                    .fill(PRIMARY),
            )
            .clicked()
        })
        .inner;
    if let Err(error) = &params {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("{}: {error}", language.text(Text::Error)),
        );
    }
    if clicked { params.ok() } else { None }
}

pub(super) fn group_heading(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_space(4.0);
    ui.vertical_centered(|ui| ui.label(egui::RichText::new(text).strong().small()))
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_scale_fields_preserve_and_accept_nanosecond_divisions() {
        let ctx = egui::Context::default();
        let mut view = PlotView::new(1e6, 2e6, -4e-9, 4e-9);
        let original = view;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 220.0));
        let mut frame = |time, events| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    scale_fields(ui, &mut view, Language::English, "s", false);
                },
            )
        };
        frame(0.0, Vec::new()).drop_without_applying_deltas();
        let output = frame(0.02, Vec::new());
        let target = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "1.000e-9" => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                _ => None,
            })
            .expect("nanosecond division must remain visible");
        output.drop_without_applying_deltas();
        frame(0.04, vec![egui::Event::PointerMoved(target)]).drop_without_applying_deltas();
        for (index, pressed) in [true, false, true, false].into_iter().enumerate() {
            frame(
                0.06 + index as f64 * 0.02,
                vec![egui::Event::PointerButton {
                    pos: target,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            )
            .drop_without_applying_deltas();
        }
        let command = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        frame(
            0.16,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: command,
                },
                egui::Event::Text("2.5e-10".into()),
                egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        )
        .drop_without_applying_deltas();
        assert_eq!((view.x_min, view.x_max), (original.x_min, original.x_max));
        assert_eq!(view.y_divisions, 8);
        assert!((view.y_max - view.y_min - 2e-9).abs() < 1e-20, "{view:?}");
    }

    #[test]
    fn explicit_range_edits_replace_x_zoom_and_preserve_manual_y_scale() {
        let caps = DEVICE_MODEL.capabilities();
        for (range, start) in [(caps.s11.range, 5e3), (caps.spec.range, 0.0)] {
            for edit in [SweepEdit::StartStop, SweepEdit::CenterSpan] {
                let mut view = PlotView::new(100e6, 200e6, -20.0, 40.0);
                view.y_divisions = 6;
                edit.sync_view(&mut view, range, start, 1e6);
                assert_eq!((view.x_min, view.x_max), (start, 1e6));
                assert_eq!((view.y_min, view.y_max, view.y_divisions), (-20.0, 40.0, 6));
            }
        }
    }

    #[test]
    fn no_edit_or_invalid_range_preserves_the_entire_view() {
        let range = DEVICE_MODEL.capabilities().s11.range;
        let original = PlotView::new(100e6, 200e6, -20.0, 40.0);
        let mut view = original;
        SweepEdit::None.sync_view(&mut view, range, 5e3, 1e6);
        assert_eq!(view, original);
        for (start, stop) in [
            (0.0, 1e6),
            (1e6, 5e3),
            (5e3, 5_001.0),
            (f64::NAN, 1e6),
            (5e3, f64::INFINITY),
        ] {
            SweepEdit::StartStop.sync_view(&mut view, range, start, stop);
            assert_eq!(view, original);
        }
    }

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
                        clicked |= run_button(ui, params, Language::English, Text::Run).is_some();
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
            .show(
                ui,
                DEVICE_MODEL.capabilities().s11.range,
                Language::English,
                "s11",
            );
            assert!(matches!(edit, SweepEdit::None));
        });
        output.drop_without_applying_deltas();
        assert_eq!(start, -1.0);
        assert_eq!(stop, 7_000_000_001.0);
        assert_eq!(points, 10001);
    }

    #[test]
    fn step_changes_point_count_without_changing_sweep_endpoints() {
        assert_eq!(points_for_step(400e6, 2e6), Some(201));
        assert_eq!(points_for_step(400e6, 3e6), Some(134));
        for step in [0.0, -1.0, f64::NAN, f64::INFINITY, 1.0, 400e6] {
            assert_eq!(points_for_step(400e6, step), None);
        }
        assert_eq!(points_for_step(-1.0, 1.0), None);
    }

    #[test]
    fn readouts_preserve_whole_hertz_precision_without_zero_padding() {
        assert_eq!(
            frequency_readout(3.500_002_5, FrequencyUnit::Gigahertz),
            "3.5000025"
        );
        assert_eq!(
            frequency_readout(1.000_000_001, FrequencyUnit::Gigahertz),
            "1.000000001"
        );
        assert_eq!(frequency_readout(1000.0, FrequencyUnit::Hz), "1000");
        assert_eq!(frequency_readout(7.0, FrequencyUnit::Gigahertz), "7");
    }

    #[test]
    fn invalid_step_stays_visible_until_corrected_or_sweep_changes() {
        let ctx = egui::Context::default();
        ctx.data_mut(|data| {
            data.insert_temp(
                step_id("s11"),
                StepDraft {
                    span: 400e6,
                    points: 201,
                    hz: -1.0,
                    invalid: true,
                },
            );
        });
        let mut points = 201;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            step_field(ui, "s11", 400e6, &mut points, Language::English);
            assert!(invalid_step(ui.ctx(), "s11"));
            assert!(!invalid_step(ui.ctx(), "spec"));
        });
        output.drop_without_applying_deltas();
        assert_eq!(points, 201);
        assert_eq!(
            ctx.data(|data| data.get_temp::<StepDraft>(step_id("s11")))
                .unwrap()
                .hz,
            -1.0
        );

        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            step_field(ui, "s11", 200e6, &mut points, Language::English);
            assert!(!invalid_step(ui.ctx(), "s11"));
        });
        output.drop_without_applying_deltas();
        assert_eq!(points, 201);
    }

    #[test]
    fn repeated_frequency_fields_fit_a_256_pixel_side_panel() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut start = 5e3;
        let mut stop = 7e9;
        let mut center = (start + stop) / 2.0;
        let mut span = stop - start;
        let mut points = 201;
        for _ in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 850.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    egui::Panel::left("sweep_test")
                        .exact_size(256.0)
                        .show(ui, |ui| {
                            let right = ui.max_rect().right();
                            SweepFields {
                                start: &mut start,
                                stop: &mut stop,
                                center: &mut center,
                                span: &mut span,
                                points: &mut points,
                            }
                            .show(
                                ui,
                                DEVICE_MODEL.capabilities().s11.range,
                                Language::English,
                                "s11",
                            );
                            assert!(
                                ui.min_rect().right() <= right + 0.1,
                                "{} > {right}",
                                ui.min_rect().right()
                            );
                        });
                },
            );
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn scale_fields_keep_three_aligned_label_value_rows() {
        for language in Language::ALL {
            for (unit, top_reference) in [("dBm", true), ("ohm", false)] {
                let ctx = egui::Context::default();
                crate::theme::setup(&ctx);
                let mut view = PlotView::new(1e6, 1e9, -50.0, 30.0);
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1280.0, 850.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        egui::Panel::right("scale_test")
                            .exact_size(256.0)
                            .frame(egui::Frame::new().inner_margin(8))
                            .show(ui, |ui| {
                                scale_fields(ui, &mut view, language, unit, top_reference);
                            });
                    },
                );
                let labels = [Text::Reference, Text::Divisions, Text::PerDivision]
                    .map(|key| language.text(key));
                let shapes = output.shapes.clone();
                output.drop_without_applying_deltas();
                let mut label_rects = Vec::new();
                let mut field_rects = Vec::new();
                for shape in &shapes {
                    if let egui::Shape::Text(text) = &shape.shape {
                        let rect = text.galley.rect.translate(text.pos.to_vec2());
                        if labels.contains(&text.galley.text()) {
                            assert_eq!(text.galley.rows.len(), 1);
                            label_rects.push(rect);
                        }
                        if text.galley.text().parse::<f64>().is_ok() {
                            field_rects.push(rect);
                        }
                    }
                }
                assert_eq!(label_rects.len(), 3);
                assert_eq!(field_rects.len(), 3);
                for (label, value) in label_rects.iter().zip(&field_rects) {
                    assert!(label.right() < value.left());
                    assert!(label.top() < value.bottom() && value.top() < label.bottom());
                    assert!(label.left() >= 1024.0 && value.right() <= 1280.0);
                }
                assert!(
                    field_rects
                        .windows(2)
                        .all(|rows| rows[0].bottom() < rows[1].top())
                );
                assert!(
                    field_rects
                        .iter()
                        .all(|rect| (rect.center().x - field_rects[0].center().x).abs() <= 1.0)
                );
                assert!(
                    label_rects
                        .iter()
                        .all(|rect| (rect.left() - label_rects[0].left()).abs() <= 0.1)
                );
            }
        }
    }
}
