// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared sweep controls and trace-specific settings.

use super::sweep_controls::{self, BUTTON_HEIGHT, SweepEdit, SweepFields, group_heading};
use crate::i18n::{Language, Text};
use crate::state::{AppState, ConnectionState, S11Display, S21Display, SweepState, WorkerCommand};
use crate::widgets::plot::{self, YScale};
use crate::workspace::{SweepMode, TraceDisplay, TraceSettings, TraceState};

pub fn show_sweep(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::FrequencyRangeTab));
    ui.add_enabled_ui(
        state.sweep != SweepState::Stopping && state.connection != ConnectionState::Disconnecting,
        |ui| {
            ui.horizontal(|ui| {
                let previous = state.workspace.sweep_mode;
                for (mode, label) in [
                    (SweepMode::Range, Text::FrequencyRange),
                    (SweepMode::List, Text::FrequencyListTab),
                    (SweepMode::Segments, Text::SegmentsTab),
                ] {
                    let enabled = (mode != SweepMode::Segments && previous != SweepMode::Segments)
                        || (state.sweep == SweepState::Idle
                            && !state.calibration.busy()
                            && !state.source.busy()
                            && !state.workspace.frequency_editor.is_pending());
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::selectable(previous == mode, state.language.text(label)),
                        )
                        .clicked()
                    {
                        if mode == SweepMode::Segments && state.workspace.segments.is_empty() {
                            state
                                .workspace
                                .segment_editor
                                .open(&[], &state.workspace.range);
                        } else {
                            state.workspace.sweep_mode = mode;
                        }
                    }
                }
                if previous != state.workspace.sweep_mode
                    && previous != SweepMode::Segments
                    && state.workspace.sweep_mode != SweepMode::Segments
                {
                    state.workspace.reset_frequency_view();
                }
            });
            if state.workspace.sweep_mode == SweepMode::Segments {
                segment_summary(ui, state);
                return;
            }
            if state.workspace.sweep_mode == SweepMode::List {
                let list = &state.workspace.frequencies_hz;
                ui.label(format!(
                    "{}: {}",
                    state.language.text(Text::Points),
                    list.len()
                ));
                if let (Some(first), Some(last)) = (list.first(), list.last()) {
                    ui.small(format!(
                        "{first} {} {last} Hz",
                        state.language.text(Text::RangeTo)
                    ));
                }
                if ui
                    .button(state.language.text(Text::EditFrequencyList))
                    .clicked()
                {
                    state.workspace.frequency_editor.open(list);
                }
                return;
            }
            let allowed = state.workspace.visible_range();
            let range = &mut state.workspace.range;
            let edit = SweepFields {
                start: &mut range.start_hz,
                stop: &mut range.stop_hz,
                center: &mut range.center_hz,
                span: &mut range.span_hz,
                points: &mut range.points,
            }
            .show(ui, allowed, state.language, "workspace");
            match edit {
                SweepEdit::StartStop => range.start_stop_changed(),
                SweepEdit::CenterSpan => range.center_span_changed(),
                SweepEdit::None => {}
            }
            edit.sync_view(
                &mut state.workspace.x_view,
                allowed,
                range.start_hz,
                range.stop_hz,
            );
        },
    );
    if state.any_running()
        && state.workspace.sweep_mode == SweepMode::Range
        && sweep_controls::invalid_step(ui.ctx(), "workspace")
    {
        state.send(WorkerCommand::StopSweep);
    }
}

fn segment_summary(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    let receivers: Vec<_> = state
        .workspace
        .traces
        .iter()
        .filter(|trace| trace.settings.visible)
        .map(|trace| (trace.id, trace.settings.receiver()))
        .collect();
    ui.label(format!(
        "{}: {}",
        language.text(Text::SegmentCount),
        state.workspace.segments.len()
    ));
    if let (Some(first), Some(last)) = (
        state.workspace.segments.first(),
        state.workspace.segments.last(),
    ) {
        ui.small(format!(
            "{} {} {}",
            plot::format_axis_value(first.start_hz as f64, "Hz"),
            language.text(Text::RangeTo),
            plot::format_axis_value(last.stop_hz as f64, "Hz")
        ));
    }
    match crate::segment_editor::validate(
        &state.workspace.segments,
        &receivers,
        state.workspace.visible_range(),
        language,
    ) {
        Ok(points) => {
            ui.label(format!(
                "{}: {points}",
                language.text(Text::SegmentAcquiredPoints)
            ));
        }
        Err(error) => {
            ui.colored_label(ui.visuals().error_fg_color, error.text);
        }
    }
    if ui.button(language.text(Text::EditSegments)).clicked() {
        state
            .workspace
            .segment_editor
            .open(&state.workspace.segments, &state.workspace.range);
    }
    if let Some((trace, preview)) = state
        .workspace
        .traces
        .iter()
        .filter(|trace| trace.settings.visible)
        .filter_map(|trace| trace.preview.as_ref().map(|preview| (trace, preview)))
        .filter(|(_, preview)| preview.segment.is_some())
        .max_by_key(|(_, preview)| preview.cycle_id)
        && let Some(progress) = preview.segment
    {
        ui.small(format!(
            "T{}  {} {}/{}  {}/{}",
            trace.id.0,
            language.text(Text::Segment),
            progress.index + 1,
            progress.count,
            progress.acquired,
            progress.expected
        ));
    }
    if let Some(snapshot) = state
        .workspace
        .selected()
        .and_then(|trace| trace.completed.as_ref())
        && snapshot.segments.is_some()
    {
        ui.small(format!(
            "{}: {}",
            language.text(Text::SegmentCompletedPoints),
            snapshot.data.points.len()
        ));
    }
    if state.workspace.traces.iter().any(|trace| {
        trace.settings.visible && trace.settings.cal == kcsdi_core::commands::Cal::CalUser
    }) {
        ui.small(language.text(Text::SegmentCalWarning));
    }
}

pub fn run_button(ui: &mut egui::Ui, state: &mut AppState) {
    let settings_width = 90.0;
    let run_width = (ui.available_width() - settings_width - ui.spacing().item_spacing.x).max(40.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(run_width, BUTTON_HEIGHT),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.add_enabled_ui(
                    state.connection == ConnectionState::Connected
                        && !state.source.busy()
                        && !state.calibration.busy(),
                    |ui| {
                        let size = [run_width, BUTTON_HEIGHT];
                        if state.sweep == SweepState::Stopping {
                            ui.add_enabled_ui(false, |ui| {
                                ui.add_sized(
                                    size,
                                    egui::Button::new(state.language.text(Text::Stopping)),
                                );
                            });
                        } else if state.any_running() {
                            let button = egui::Button::new(
                                egui::RichText::new(state.language.text(Text::StopSweep)).strong(),
                            )
                            .fill(egui::Color32::from_rgb(0xd3, 0x2f, 0x2f));
                            if ui.add_sized(size, button).clicked() {
                                state.send(WorkerCommand::StopSweep);
                            }
                        } else {
                            let plan = if state.workspace.sweep_mode == SweepMode::Range
                                && sweep_controls::invalid_step(ui.ctx(), "workspace")
                            {
                                Err(kcsdi_core::Error::InvalidParameter(
                                    state.language.text(Text::InvalidStep).into(),
                                ))
                            } else {
                                state.workspace.plan()
                            };
                            let caption = if state.workspace.run.recording.enabled {
                                Text::RunAndSave
                            } else {
                                Text::Run
                            };
                            if let Some(plan) =
                                sweep_controls::run_button(ui, plan, state.language, caption)
                            {
                                state.send(WorkerCommand::RunWorkspace(plan));
                            }
                        }
                    },
                );
            },
        );
        if ui
            .add_enabled_ui(!state.workspace.run_editor.is_pending(), |ui| {
                ui.add_sized(
                    [settings_width, BUTTON_HEIGHT],
                    egui::Button::new(state.language.text(Text::Settings)),
                )
            })
            .inner
            .on_hover_text(state.language.text(Text::RunSettings))
            .on_disabled_hover_text(state.language.text(Text::RunFilePending))
            .clicked()
        {
            state.workspace.run_editor.open(&state.workspace.run);
        }
    });
}

pub fn format_fields(ui: &mut egui::Ui, settings: &mut TraceSettings, language: Language) {
    egui::ComboBox::from_id_salt("trace_display")
        .selected_text(settings.display.label(language))
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut settings.display,
                TraceDisplay::Spec,
                language.text(Text::Spectrum),
            );
            for display in S11Display::ALL {
                ui.selectable_value(
                    &mut settings.display,
                    TraceDisplay::S11(display),
                    format!("S11 {}", display.label(language)),
                );
            }
            for display in S21Display::ALL {
                ui.selectable_value(
                    &mut settings.display,
                    TraceDisplay::S21(display),
                    format!("S21 {}", display.label(language)),
                );
            }
        });
}

pub fn receiver_fields(ui: &mut egui::Ui, settings: &mut TraceSettings, language: Language) {
    super::s11_panel::receiver_fields(ui, settings, language);
    match settings.display {
        TraceDisplay::Spec => super::spec_panel::receiver_fields(ui, settings, language),
        TraceDisplay::S21(_) => super::spec_panel::lo_fields(ui, settings, language),
        TraceDisplay::S11(_) => {}
    }
}

pub fn settings_fields(ui: &mut egui::Ui, settings: &mut TraceSettings, language: Language) {
    format_fields(ui, settings, language);
    receiver_fields(ui, settings, language);
    ui.horizontal(|ui| {
        ui.label(language.text(Text::Color));
        ui.color_edit_button_srgba(&mut settings.color);
    });
    ui.horizontal(|ui| {
        ui.label(language.text(Text::LineWidth));
        ui.add(
            egui::DragValue::new(&mut settings.line_width)
                .speed(0.1)
                .range(0.5..=5.0),
        );
    });
}

pub fn log_y_control(ui: &mut egui::Ui, trace: &mut TraceState, language: Language) {
    if let Some(scale) = trace.settings.display.logarithmic_y() {
        let mut enabled = trace.view.y_scale != YScale::Linear;
        let help = if scale == YScale::LogImpedance {
            Text::LogYHelp
        } else {
            Text::LogVswrHelp
        };
        if ui
            .checkbox(&mut enabled, language.text(Text::LogY))
            .on_hover_text(language.text(help))
            .changed()
        {
            trace.view.y_scale = if enabled { scale } else { YScale::Linear };
            trace.view.ensure_y_view();
            trace.view_locked = false;
            trace.needs_fit = true;
        }
    }
}

pub fn display_fields(ui: &mut egui::Ui, trace: &mut TraceState, language: Language) {
    if trace.settings.display.is_smith() {
        return;
    }
    // A logarithmic axis has no constant value per division.
    if trace.view.y_scale == YScale::Linear
        && sweep_controls::scale_fields(
            ui,
            &mut trace.view,
            language,
            trace.settings.display.unit(),
            trace.settings.display == TraceDisplay::Spec,
        )
    {
        trace.view_locked = true;
        trace.needs_fit = false;
    }
}

#[derive(Clone, Copy, PartialEq)]
struct AxisRange {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
}

impl AxisRange {
    fn valid(self, log_x: bool, y_scale: YScale) -> bool {
        let valid_span = |min: f64, max: f64| {
            min.is_finite() && max.is_finite() && min < max && (max - min).is_finite()
        };
        valid_span(self.x_min, self.x_max)
            && valid_span(self.y_min, self.y_max)
            && (self.y_min + self.y_max).is_finite()
            && (!log_x || (self.x_min > 0.0 && self.x_max.log10() > self.x_min.log10()))
            && y_scale
                .floor()
                .is_none_or(|floor| self.y_min >= floor && self.y_max.log10() > self.y_min.log10())
    }
}

#[derive(Clone, Copy)]
struct RangeDraft {
    source: AxisRange,
    values: AxisRange,
    log_x: bool,
    y_scale: YScale,
    display: TraceDisplay,
}

/// Display bounds are independent of the acquisition range and point grid.
pub fn view_fields(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    let workspace = &mut state.workspace;
    let log_x = workspace.log_x;
    let x_view = workspace.x_view;
    let Some(trace) = workspace.selected_mut() else {
        return;
    };
    if trace.settings.display.is_smith() {
        return;
    }
    let source = AxisRange {
        x_min: x_view.x_min,
        x_max: x_view.x_max,
        y_min: trace.view.y_min,
        y_max: trace.view.y_max,
    };
    let y_scale = trace.view.y_scale;
    let id = ui.id().with(("axis_range", trace.id.0));
    let mut draft = ui
        .ctx()
        .data(|data| data.get_temp::<RangeDraft>(id))
        .filter(|draft| {
            (draft.source == source || draft.values != draft.source)
                && draft.log_x == log_x
                && draft.y_scale == y_scale
                && draft.display == trace.settings.display
        })
        .unwrap_or(RangeDraft {
            source,
            values: source,
            log_x,
            y_scale,
            display: trace.settings.display,
        });
    let mut applied = false;
    let mut fit = false;
    let response = egui::CollapsingHeader::new(language.text(Text::AxisRange))
        .id_salt(id)
        .show(ui, |ui| {
            egui::Grid::new(id.with("fields"))
                .num_columns(2)
                .show(ui, |ui| {
                    for (label, value, unit) in [
                        (Text::XMinimum, &mut draft.values.x_min, "Hz"),
                        (Text::XMaximum, &mut draft.values.x_max, "Hz"),
                        (
                            Text::YMinimum,
                            &mut draft.values.y_min,
                            trace.settings.display.unit(),
                        ),
                        (
                            Text::YMaximum,
                            &mut draft.values.y_max,
                            trace.settings.display.unit(),
                        ),
                    ] {
                        ui.label(language.text(label));
                        let speed =
                            (value.abs() * 0.01).max(if unit == "s" { 1e-15 } else { 1e-6 });
                        ui.add(
                            egui::DragValue::new(value)
                                .speed(speed)
                                .max_decimals(15)
                                .custom_formatter(move |value, _| {
                                    plot::format_axis_value(value, unit)
                                })
                                .custom_parser(move |text| plot::parse_axis_value(text, unit)),
                        );
                        ui.end_row();
                    }
                });
            let valid = draft.values.valid(log_x, y_scale);
            if !valid {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    language.text(Text::InvalidAxisRange),
                );
            }
            applied = ui
                .add_enabled(valid, egui::Button::new(language.text(Text::ApplyRange)))
                .clicked();
            fit = ui.button(language.text(Text::FitRange)).clicked();
        });
    response
        .header_response
        .on_hover_text(language.text(Text::AxisRangeHelp));
    if applied {
        trace.view.y_min = draft.values.y_min;
        trace.view.y_max = draft.values.y_max;
        trace.view_locked = true;
        trace.needs_fit = false;
        workspace.x_view.x_min = draft.values.x_min;
        workspace.x_view.x_max = draft.values.x_max;
        draft.source = draft.values;
    } else if fit {
        let (low, high) = trace.settings.display.default_y();
        trace.view.reset(source.x_min, source.x_max, low, high);
        trace.view_locked = false;
        trace.needs_fit = true;
        workspace.reset_frequency_view();
    }
    ui.ctx().data_mut(|data| {
        if fit {
            data.remove::<RangeDraft>(id);
        } else {
            data.insert_temp(id, draft);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_ranges_validate_units_floors_and_finite_spans() {
        let base = AxisRange {
            x_min: 5e3,
            x_max: 1e9,
            y_min: 1e-3,
            y_max: 1e3,
        };
        assert!(base.valid(true, YScale::LogImpedance));
        assert!(!base.valid(true, YScale::LogVswr));
        assert!(AxisRange { y_min: 1.0, ..base }.valid(true, YScale::LogVswr));
        let signed = AxisRange {
            y_min: -100.0,
            ..base
        };
        assert!(signed.valid(true, YScale::Linear));
        assert!(!signed.valid(true, YScale::LogImpedance));
        let dc = AxisRange { x_min: 0.0, ..base };
        assert!(dc.valid(false, YScale::LogImpedance));
        assert!(!dc.valid(true, YScale::LogImpedance));
        for invalid in [
            AxisRange {
                x_max: base.x_min,
                ..base
            },
            AxisRange {
                y_max: base.y_min,
                ..base
            },
            AxisRange {
                x_min: f64::NAN,
                ..base
            },
            AxisRange {
                y_min: f64::NEG_INFINITY,
                ..base
            },
            AxisRange {
                y_max: f64::INFINITY,
                ..base
            },
            AxisRange {
                y_min: -f64::MAX,
                y_max: f64::MAX,
                ..base
            },
        ] {
            assert!(!invalid.valid(false, YScale::Linear));
        }
        assert!(
            AxisRange {
                y_min: -2e-18,
                y_max: 3e-18,
                ..base
            }
            .valid(false, YScale::Linear)
        );
    }

    #[test]
    fn display_controls_offer_log_y_only_for_impedance_and_vswr() {
        for display in [TraceDisplay::Spec]
            .into_iter()
            .chain(S11Display::ALL.map(TraceDisplay::S11))
            .chain(S21Display::ALL.map(TraceDisplay::S21))
        {
            let ctx = egui::Context::default();
            let mut trace = TraceState::new(
                crate::acquisition::TraceId(1),
                TraceSettings {
                    display,
                    ..Default::default()
                },
                &Default::default(),
            );
            let output = ctx.run_ui(Default::default(), |ui| {
                log_y_control(ui, &mut trace, Language::English)
            });
            let log_y = output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "LOG Y"));
            assert_eq!(log_y, display.logarithmic_y().is_some(), "{display:?}");
            assert_eq!(trace.view.y_scale, YScale::Linear);
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn run_and_settings_captions_fit_the_parameter_panel() {
        for language in Language::ALL {
            for (sweep, recording, caption) in [
                (SweepState::Idle, false, Text::Run),
                (SweepState::Idle, true, Text::RunAndSave),
                (SweepState::Running, true, Text::StopSweep),
                (SweepState::Stopping, true, Text::Stopping),
            ] {
                let ctx = egui::Context::default();
                crate::theme::setup(&ctx);
                let mut state = AppState {
                    language,
                    connection: ConnectionState::Connected,
                    sweep,
                    ..Default::default()
                };
                state.workspace.run.recording.enabled = recording;
                state.workspace.run.recording.directory = std::env::temp_dir();
                for _ in 0..3 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(960.0, 600.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            egui::Panel::right("run_test_panel")
                                .exact_size(256.0)
                                .show(ui, |ui| {
                                    run_button(ui, &mut state);
                                });
                        },
                    );
                    let panel = egui::containers::panel::PanelState::load(
                        &ctx,
                        egui::Id::new("run_test_panel"),
                    )
                    .unwrap();
                    assert!(
                        (panel.outer_rect.left() - (960.0 - 256.0)).abs() <= 1.0,
                        "{language:?} {caption:?}: {:?}",
                        panel.outer_rect
                    );
                    for key in [caption, Text::Settings] {
                        let shape = output
                            .shapes
                            .iter()
                            .find_map(|shape| {
                                if let egui::Shape::Text(text) = &shape.shape
                                    && text.galley.job.text == language.text(key)
                                {
                                    Some((shape.clip_rect, text))
                                } else {
                                    None
                                }
                            })
                            .expect("run control caption");
                        let bounds = shape.1.galley.rect.translate(shape.1.pos.to_vec2());
                        assert!(
                            shape.0.contains_rect(bounds),
                            "{language:?} {key:?}: {bounds:?}"
                        );
                    }
                    output.drop_without_applying_deltas();
                }
            }
        }
    }

    #[test]
    fn run_settings_can_open_while_disconnected() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut state = AppState {
            language: Language::English,
            connection: ConnectionState::Disconnected,
            ..Default::default()
        };
        let mut pointer = egui::Pos2::ZERO;
        for frame in 0..6 {
            let events = if frame == 3 || frame == 4 {
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed: frame == 3,
                        modifiers: egui::Modifiers::default(),
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
                    time: Some(frame as f64 / 30.0),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::Panel::right("run_offline_test")
                        .exact_size(256.0)
                        .show(ui, |ui| run_button(ui, &mut state));
                    state.workspace.run_editor.show(ui.ctx(), state.language);
                },
            );
            let captions: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && [Text::Settings, Text::RunSettings]
                            .iter()
                            .any(|&key| text.galley.job.text == state.language.text(key))
                    {
                        Some(text.galley.rect.translate(text.pos.to_vec2()))
                    } else {
                        None
                    }
                })
                .collect();
            if frame == 2 {
                pointer = captions[0].center();
            }
            if frame == 5 {
                assert_eq!(captions.len(), 2, "settings button and open modal heading");
                assert_eq!(state.sweep, SweepState::Idle);
                assert_eq!(state.connection, ConnectionState::Disconnected);
            }
            output.drop_without_applying_deltas();
        }
    }
}
