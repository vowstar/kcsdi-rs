// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared sweep controls and trace-specific settings.

use super::sweep_controls::{self, BUTTON_HEIGHT, SweepEdit, SweepFields, group_heading};
use crate::i18n::{Language, Text};
use crate::state::{AppState, ConnectionState, S11Display, S21Display, SweepState, WorkerCommand};
use crate::workspace::{TraceDisplay, TraceSettings, TraceState};

pub fn show_sweep(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::FrequencyRangeTab));
    ui.add_enabled_ui(
        state.sweep != SweepState::Stopping && state.connection != ConnectionState::Disconnecting,
        |ui| {
            ui.horizontal(|ui| {
                let previous = state.workspace.list_mode;
                ui.selectable_value(
                    &mut state.workspace.list_mode,
                    false,
                    state.language.text(Text::FrequencyRange),
                );
                ui.selectable_value(
                    &mut state.workspace.list_mode,
                    true,
                    state.language.text(Text::FrequencyListTab),
                );
                if previous != state.workspace.list_mode {
                    state.workspace.reset_frequency_view();
                }
            });
            if state.workspace.list_mode {
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
        && !state.workspace.list_mode
        && sweep_controls::invalid_step(ui.ctx(), "workspace")
    {
        state.send(WorkerCommand::StopSweep);
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
                            let plan = if !state.workspace.list_mode
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

pub fn display_fields(ui: &mut egui::Ui, trace: &mut TraceState, language: Language) {
    if trace.settings.display.is_smith() {
        return;
    }
    if sweep_controls::scale_fields(
        ui,
        &mut trace.view,
        language,
        trace.settings.display.unit(),
        trace.settings.display == TraceDisplay::Spec,
    ) {
        trace.view_locked = true;
        trace.needs_fit = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
