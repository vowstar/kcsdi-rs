// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Right column: SPEC sweep parameters and run control.

use crate::i18n::Text;
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, WorkerCommand};

use super::sweep_controls::{self, SweepEdit, SweepFields, group_heading};

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Draw the SPEC parameter panel. Signature is a module contract; do not
/// change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let connected = state.connection == ConnectionState::Connected;
    let running = state.spec.running;

    ui.add_enabled_ui(connected, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            group_heading(ui, state.language.text(Text::Sweep));
            // Parameter edits are locked while a sweep is running.
            ui.add_enabled_ui(!running, |ui| {
                sweep_fields(ui, state);
                ui.add_space(8.0);
                receiver_fields(ui, state);
            });

            group_heading(ui, state.language.text(Text::Display));
            if crate::widgets::plot::log_x_control(ui, &mut state.spec.log_x) {
                state.spec.needs_fit = true;
                state.spec.view_locked = false;
            }

            ui.add_space(16.0);
            run_button(ui, state, running);
        });
    });
}

/// START/STOP/CENTER/SPAN frequency fields plus the POINTS count.
fn sweep_fields(ui: &mut egui::Ui, state: &mut AppState) {
    let spec = &mut state.spec;
    let edit = SweepFields {
        start: &mut spec.start_hz,
        stop: &mut spec.stop_hz,
        center: &mut spec.center_hz,
        span: &mut spec.span_hz,
        points: &mut spec.points,
    }
    .show(ui, DEVICE_MODEL.capabilities().spec.range, state.language);
    match edit {
        SweepEdit::StartStop => spec.start_stop_changed(),
        SweepEdit::CenterSpan => spec.center_span_changed(),
        SweepEdit::None => {}
    }
}

/// RBW selector and reference level.
fn receiver_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::Receiver));
    egui::Grid::new("receiver_grid")
        .num_columns(2)
        .spacing([8.0, 8.0])
        .show(ui, |ui| {
            let spec = &mut state.spec;
            ui.label(state.language.text(Text::Rbw));
            egui::ComboBox::from_id_salt("rbw")
                .selected_text(spec.rbw.to_string())
                .show_ui(ui, |ui| {
                    for &rbw in DEVICE_MODEL.capabilities().rbw_list {
                        ui.selectable_value(&mut spec.rbw, rbw, rbw.to_string());
                    }
                });
            ui.end_row();
            ui.label(state.language.text(Text::RefLevel));
            ui.add(
                egui::DragValue::new(&mut spec.ref_level_dbm)
                    .range(
                        DEVICE_MODEL.capabilities().spec.ref_min_dbm
                            ..=DEVICE_MODEL.capabilities().spec.ref_max_dbm,
                    )
                    .clamp_existing_to_range(false)
                    .suffix(" dBm"),
            );
            ui.end_row();
        });
}

/// Full-width RUN/STOP toggle, green/primary at rest and red while running.
fn run_button(ui: &mut egui::Ui, state: &mut AppState, running: bool) {
    let size = [ui.available_width(), 32.0];
    if running {
        let button =
            egui::Button::new(egui::RichText::new(state.language.text(Text::StopSweep)).strong())
                .fill(RED);
        if ui.add_sized(size, button).clicked() {
            state.spec.running = false;
            state.send(WorkerCommand::StopSweep);
        }
    } else if let Some(params) =
        sweep_controls::run_button(ui, state.spec.spec_params(), state.language)
    {
        state.send(WorkerCommand::RunSpec(params));
        state.spec.running = true;
        // Auto-fit the view to the incoming sweep data.
        state.spec.needs_fit = true;
        state.spec.view_locked = false;
    }
}
