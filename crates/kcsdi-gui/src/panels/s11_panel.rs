// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! S11 sweep, trace editor, and receiver controls.

use kcsdi_core::commands::Cal;

use crate::i18n::{Language, StatusMessage, Text};
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, S11Display, WorkerCommand};

use super::sweep_controls::{
    self, BUTTON_HEIGHT, SweepEdit, SweepFields, choice_button, group_heading,
};

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

fn cal_label(cal: Cal, language: Language) -> &'static str {
    language.text(match cal {
        Cal::CalOn => Text::CalOn,
        Cal::CalOff => Text::CalOff,
        Cal::CalSys => Text::CalSys,
        Cal::CalUser => Text::CalUser,
    })
}

/// Draw the run and receiver controls at the top of the right pane.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let connected = state.connection == ConnectionState::Connected;
    let running = state.s11.running;

    ui.add_enabled_ui(connected, |ui| {
        run_button(ui, state, running);
    });
    ui.add_enabled_ui(!running, |ui| {
        receiver_fields(ui, state);
    });
}

/// Draw scale and display controls below the hold and marker controls.
pub fn show_display_controls(ui: &mut egui::Ui, state: &mut AppState) {
    if state.s11.display != S11Display::Smith {
        if sweep_controls::scale_fields(
            ui,
            &mut state.s11.view,
            state.language,
            state.s11.display.y_label(),
            false,
        ) {
            state.s11.view_locked = true;
            state.s11.needs_fit = false;
        }
        display_fields(ui, state);
    }
}

/// Segmented display-format selector: Phase / Return Loss / VSWR / Smith /
/// Impedance. Switching re-fits the view because the Y range changes.
pub fn show_trace_editor(ui: &mut egui::Ui, state: &mut AppState) {
    let previous = state.s11.display;
    ui.horizontal_wrapped(|ui| {
        for display in S11Display::ALL {
            if ui
                .selectable_value(
                    &mut state.s11.display,
                    display,
                    display.label(state.language),
                )
                .changed()
            {
                state.s11.needs_fit = true;
                state.s11.view_locked = false;
                let (y_min, y_max) = display.default_y();
                state
                    .s11
                    .view
                    .reset(state.s11.start_hz, state.s11.stop_hz, y_min, y_max);
            }
        }
    });
    if previous.wire_format() != state.s11.display.wire_format() {
        if state.s11.running {
            match state.s11.s11_params() {
                Ok(params) => state.send(WorkerCommand::RunS11(params)),
                Err(error) => {
                    state.s11.running = false;
                    state.send(WorkerCommand::StopSweep);
                    state.status_message = Some(error.to_string().into());
                }
            }
        } else {
            state.status_message = Some(StatusMessage::Text(Text::RunForDisplay));
        }
    }
}

/// Draw linked frequency controls in the left pane, also while offline.
pub fn show_sweep(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::FrequencyRangeTab));
    ui.add_enabled_ui(!state.s11.running, |ui| {
        let s11 = &mut state.s11;
        let edit = SweepFields {
            start: &mut s11.start_hz,
            stop: &mut s11.stop_hz,
            center: &mut s11.center_hz,
            span: &mut s11.span_hz,
            points: &mut s11.points,
        }
        .show(
            ui,
            DEVICE_MODEL.capabilities().s11.range,
            state.language,
            "s11",
        );
        match edit {
            SweepEdit::StartStop => s11.start_stop_changed(),
            SweepEdit::CenterSpan => s11.center_span_changed(),
            SweepEdit::None => {}
        }
        edit.sync_view(
            &mut s11.view,
            DEVICE_MODEL.capabilities().s11.range,
            s11.start_hz,
            s11.stop_hz,
        );
    });
}

/// CAL selector and the optional RBW pushed before a run.
fn receiver_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::Calibration));
    let calibrations = DEVICE_MODEL.capabilities().s11_calibrations();
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 8.0 * (calibrations.len() - 1) as f32)
            / calibrations.len() as f32;
        for &cal in calibrations {
            if choice_button(
                ui,
                cal_label(cal, state.language),
                state.s11.cal == cal,
                width,
            ) {
                state.s11.cal = cal;
            }
        }
    });
    group_heading(ui, state.language.text(Text::Rbw))
        .on_hover_text(state.language.text(Text::RbwDefaultHelp));
    let values = DEVICE_MODEL.capabilities().rbw_list;
    for row in values.chunks(4) {
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0 * (row.len() - 1) as f32) / row.len() as f32;
            for &rbw in row {
                if choice_button(ui, &rbw.to_string(), state.s11.rbw == Some(rbw), width) {
                    // No selection preserves the instrument's current RBW.
                    state.s11.rbw = (state.s11.rbw != Some(rbw)).then_some(rbw);
                }
            }
        });
    }
}

/// Frequency-axis display control, independent of sweep sampling.
fn display_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::Display));
    ui.add_enabled_ui(state.s11.display != S11Display::Smith, |ui| {
        if crate::widgets::plot::log_x_control(ui, &mut state.s11.log_x) {
            state.s11.needs_fit = true;
            state.s11.view_locked = false;
        }
    });
}

/// Full-width RUN/STOP toggle, green/primary at rest and red while running.
fn run_button(ui: &mut egui::Ui, state: &mut AppState, running: bool) {
    let size = [ui.available_width(), BUTTON_HEIGHT];
    if running {
        let button =
            egui::Button::new(egui::RichText::new(state.language.text(Text::StopSweep)).strong())
                .fill(RED);
        if ui.add_sized(size, button).clicked() {
            state.s11.running = false;
            state.send(WorkerCommand::StopSweep);
        }
    } else {
        let params = if sweep_controls::invalid_step(ui.ctx(), "s11") {
            Err(kcsdi_core::Error::InvalidParameter(
                state.language.text(Text::InvalidStep).into(),
            ))
        } else {
            state.s11.s11_params()
        };
        if let Some(params) = sweep_controls::run_button(ui, params, state.language) {
            state.send(WorkerCommand::RunS11(params));
            state.s11.running = true;
            state.s11.needs_fit = !state.s11.view_locked;
        }
    }
}
