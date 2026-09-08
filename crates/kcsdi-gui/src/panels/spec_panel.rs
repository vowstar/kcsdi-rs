// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Spectrum sweep and receiver controls.

use crate::i18n::Text;
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, WorkerCommand};

use super::sweep_controls::{
    self, BUTTON_HEIGHT, SweepEdit, SweepFields, choice_button, group_heading,
};

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Draw the run and receiver controls at the top of the right pane.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let connected = state.connection == ConnectionState::Connected;
    let running = state.spec.running;

    ui.add_enabled_ui(connected, |ui| {
        run_button(ui, state, running);
    });
    ui.add_enabled_ui(!running, |ui| receiver_fields(ui, state));
}

/// Draw scale and display controls below the hold and marker controls.
pub fn show_display_controls(ui: &mut egui::Ui, state: &mut AppState) {
    if sweep_controls::scale_fields(ui, &mut state.spec.view, state.language, "dBm", true) {
        state.spec.view_locked = true;
        state.spec.needs_fit = false;
    }
    group_heading(ui, state.language.text(Text::Display));
    if crate::widgets::plot::log_x_control(ui, &mut state.spec.log_x) {
        state.spec.needs_fit = true;
        state.spec.view_locked = false;
    }
}

/// Draw linked frequency controls in the left pane, also while offline.
pub fn show_sweep(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::FrequencyRangeTab));
    ui.add_enabled_ui(!state.spec.running, |ui| {
        let spec = &mut state.spec;
        let edit = SweepFields {
            start: &mut spec.start_hz,
            stop: &mut spec.stop_hz,
            center: &mut spec.center_hz,
            span: &mut spec.span_hz,
            points: &mut spec.points,
        }
        .show(
            ui,
            DEVICE_MODEL.capabilities().spec.range,
            state.language,
            "spec",
        );
        match edit {
            SweepEdit::StartStop => spec.start_stop_changed(),
            SweepEdit::CenterSpan => spec.center_span_changed(),
            SweepEdit::None => {}
        }
        edit.sync_view(
            &mut spec.view,
            DEVICE_MODEL.capabilities().spec.range,
            spec.start_hz,
            spec.stop_hz,
        );
    });
}

/// RBW selector and reference level.
fn receiver_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::Rbw));
    for row in DEVICE_MODEL.capabilities().rbw_list.chunks(4) {
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0 * (row.len() - 1) as f32) / row.len() as f32;
            for &rbw in row {
                if choice_button(ui, &rbw.to_string(), state.spec.rbw == rbw, width) {
                    state.spec.rbw = rbw;
                }
            }
        });
    }
    group_heading(ui, state.language.text(Text::RefLevel));
    ui.add(
        egui::DragValue::new(&mut state.spec.ref_level_dbm)
            .range(
                DEVICE_MODEL.capabilities().spec.ref_min_dbm
                    ..=DEVICE_MODEL.capabilities().spec.ref_max_dbm,
            )
            .clamp_existing_to_range(false)
            .suffix(" dBm"),
    );
}

/// Full-width RUN/STOP toggle, green/primary at rest and red while running.
fn run_button(ui: &mut egui::Ui, state: &mut AppState, running: bool) {
    let size = [ui.available_width(), BUTTON_HEIGHT];
    if running {
        let button =
            egui::Button::new(egui::RichText::new(state.language.text(Text::StopSweep)).strong())
                .fill(RED);
        if ui.add_sized(size, button).clicked() {
            state.spec.running = false;
            state.send(WorkerCommand::StopSweep);
        }
    } else {
        let params = if sweep_controls::invalid_step(ui.ctx(), "spec") {
            Err(kcsdi_core::Error::InvalidParameter(
                state.language.text(Text::InvalidStep).into(),
            ))
        } else {
            state.spec.spec_params()
        };
        if let Some(params) = sweep_controls::run_button(ui, params, state.language) {
            state.send(WorkerCommand::RunSpec(params));
            state.spec.running = true;
            state.spec.needs_fit = !state.spec.view_locked;
        }
    }
}
