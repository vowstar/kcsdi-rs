// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Right column: S11 display format tabs, sweep parameters, run control.

use kcsdi_core::commands::Cal;
use kcsdi_core::model::Rbw;

use crate::i18n::{Language, StatusMessage, Text};
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, S11Display, WorkerCommand};

use super::sweep_controls::{self, SweepEdit, SweepFields, group_heading};

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

fn cal_label(cal: Cal, language: Language) -> &'static str {
    language.text(match cal {
        Cal::CalOn => Text::CalOn,
        Cal::CalOff => Text::CalOff,
        Cal::CalSys => Text::CalSys,
        Cal::CalUser => Text::CalUser,
    })
}

/// Draw the S11 parameter panel. Signature is a module contract; do not
/// change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let connected = state.connection == ConnectionState::Connected;
    let running = state.s11.running;

    ui.add_enabled_ui(connected, |ui| {
        display_tabs(ui, state);
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            group_heading(ui, state.language.text(Text::Sweep));
            // Parameter edits are locked while a sweep is running.
            ui.add_enabled_ui(!running, |ui| {
                sweep_fields(ui, state);
                ui.add_space(8.0);
                receiver_fields(ui, state);
            });
            ui.add_space(8.0);
            display_fields(ui, state);

            ui.add_space(16.0);
            run_button(ui, state, running);
        });
    });
}

/// Segmented display-format selector: Phase / Return Loss / VSWR / Smith /
/// Impedance. Switching re-fits the view because the Y range changes.
fn display_tabs(ui: &mut egui::Ui, state: &mut AppState) {
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

/// START/STOP/CENTER/SPAN frequency fields plus the POINTS count.
fn sweep_fields(ui: &mut egui::Ui, state: &mut AppState) {
    let s11 = &mut state.s11;
    let edit = SweepFields {
        start: &mut s11.start_hz,
        stop: &mut s11.stop_hz,
        center: &mut s11.center_hz,
        span: &mut s11.span_hz,
        points: &mut s11.points,
    }
    .show(ui, DEVICE_MODEL.capabilities().s11.range, state.language);
    match edit {
        SweepEdit::StartStop => s11.start_stop_changed(),
        SweepEdit::CenterSpan => s11.center_span_changed(),
        SweepEdit::None => {}
    }
}

/// CAL selector and the optional RBW pushed before a run.
fn receiver_fields(ui: &mut egui::Ui, state: &mut AppState) {
    group_heading(ui, state.language.text(Text::Receiver));
    egui::Grid::new("s11_receiver_grid")
        .num_columns(2)
        .spacing([8.0, 8.0])
        .show(ui, |ui| {
            let s11 = &mut state.s11;
            ui.label(state.language.text(Text::Calibration));
            egui::ComboBox::from_id_salt("s11_cal")
                .selected_text(cal_label(s11.cal, state.language))
                .show_ui(ui, |ui| {
                    for &cal in DEVICE_MODEL.capabilities().s11_calibrations() {
                        ui.selectable_value(&mut s11.cal, cal, cal_label(cal, state.language))
                            .on_hover_text(cal.as_str());
                    }
                });
            ui.end_row();

            // RBW is optional on S11 runs: unchecked means no `$bw` push.
            let mut rbw_on = s11.rbw.is_some();
            if ui
                .checkbox(&mut rbw_on, state.language.text(Text::Rbw))
                .changed()
            {
                s11.rbw = rbw_on.then_some(Rbw::R10k);
            }
            if let Some(rbw) = &mut s11.rbw {
                egui::ComboBox::from_id_salt("s11_rbw")
                    .selected_text(rbw.to_string())
                    .show_ui(ui, |ui| {
                        for &value in DEVICE_MODEL.capabilities().rbw_list {
                            ui.selectable_value(rbw, value, value.to_string());
                        }
                    });
            }
            ui.end_row();
        });
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
    let size = [ui.available_width(), 32.0];
    if running {
        let button =
            egui::Button::new(egui::RichText::new(state.language.text(Text::StopSweep)).strong())
                .fill(RED);
        if ui.add_sized(size, button).clicked() {
            state.s11.running = false;
            state.send(WorkerCommand::StopSweep);
        }
    } else if let Some(params) =
        sweep_controls::run_button(ui, state.s11.s11_params(), state.language)
    {
        state.send(WorkerCommand::RunS11(params));
        state.s11.running = true;
        state.s11.needs_fit = true;
        state.s11.view_locked = false;
    }
}
