// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Right column: SPEC sweep parameters and run control.

use kcsdi_core::model::Rbw;

use crate::state::{AppState, ConnectionState, WorkerCommand};
use crate::theme::PRIMARY;

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Upper frequency bound for input fields in MHz (KC901V tops at 6.8 GHz).
const MAX_MHZ: f64 = 6800.0;

/// Draw the SPEC parameter panel. Signature is a module contract; do not
/// change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let connected = state.connection == ConnectionState::Connected;
    let running = state.spec.running;

    ui.add_enabled_ui(connected, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            group_heading(ui, "SWEEP");
            // Parameter edits are locked while a sweep is running.
            ui.add_enabled_ui(!running, |ui| {
                sweep_fields(ui, state);
                ui.add_space(8.0);
                receiver_fields(ui, state);
            });

            group_heading(ui, "DISPLAY");
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
    let mut start_stop_edited = false;
    let mut center_span_edited = false;

    egui::Grid::new("sweep_grid")
        .num_columns(2)
        .spacing([8.0, 8.0])
        .show(ui, |ui| {
            let spec = &mut state.spec;
            ui.label("START");
            start_stop_edited |= freq_field(ui, &mut spec.start_hz);
            ui.end_row();
            ui.label("STOP");
            start_stop_edited |= freq_field(ui, &mut spec.stop_hz);
            ui.end_row();
            ui.label("CENTER");
            center_span_edited |= freq_field(ui, &mut spec.center_hz);
            ui.end_row();
            ui.label("SPAN");
            center_span_edited |= freq_field(ui, &mut spec.span_hz);
            ui.end_row();
            ui.label("POINTS");
            ui.add(egui::DragValue::new(&mut spec.points).range(2..=10001));
            ui.end_row();
        });

    if start_stop_edited {
        state.spec.start_stop_changed();
    } else if center_span_edited {
        state.spec.center_span_changed();
    }
}

/// RBW selector and reference level.
fn receiver_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, "RECEIVER");
    egui::Grid::new("receiver_grid")
        .num_columns(2)
        .spacing([8.0, 8.0])
        .show(ui, |ui| {
            let spec = &mut state.spec;
            ui.label("RBW");
            egui::ComboBox::from_id_salt("rbw")
                .selected_text(spec.rbw.to_string())
                .show_ui(ui, |ui| {
                    for rbw in Rbw::ALL {
                        ui.selectable_value(&mut spec.rbw, rbw, rbw.to_string());
                    }
                });
            ui.end_row();
            ui.label("REF LEVEL");
            ui.add(
                egui::DragValue::new(&mut spec.ref_level_dbm)
                    .range(-30..=0)
                    .suffix(" dBm"),
            );
            ui.end_row();
        });
}

/// Full-width RUN/STOP toggle, green/primary at rest and red while running.
fn run_button(ui: &mut egui::Ui, state: &mut AppState, running: bool) {
    let size = [ui.available_width(), 32.0];
    if running {
        let button = egui::Button::new(egui::RichText::new("STOP").strong()).fill(RED);
        if ui.add_sized(size, button).clicked() {
            state.spec.running = false;
            state.send(WorkerCommand::StopSweep);
        }
    } else {
        let button = egui::Button::new(egui::RichText::new("RUN").strong()).fill(PRIMARY);
        if ui.add_sized(size, button).clicked() {
            state.send(WorkerCommand::RunSpec(state.spec.spec_params()));
            state.spec.running = true;
            // Auto-fit the view to the incoming sweep data.
            state.spec.needs_fit = true;
            state.spec.view_locked = false;
        }
    }
}

/// Centered uppercase group header matching the reference menu groups.
fn group_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(text).strong().small());
    });
    ui.separator();
}

/// Frequency input in MHz over an `f64` value in Hz. Returns true when the
/// value changed this frame.
fn freq_field(ui: &mut egui::Ui, hz: &mut f64) -> bool {
    let mut mhz = *hz / 1e6;
    let changed = ui
        .add(
            egui::DragValue::new(&mut mhz)
                .speed(0.1)
                .range(0.0..=MAX_MHZ)
                .suffix(" MHz")
                .max_decimals(3),
        )
        .changed();
    if changed {
        *hz = mhz * 1e6;
    }
    changed
}
