// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Right column: S11 display format tabs, sweep parameters, run control.

use kcsdi_core::commands::Cal;
use kcsdi_core::model::Rbw;

use crate::i18n::{Language, StatusMessage, Text};
use crate::state::{AppState, ConnectionState, S11Display, WorkerCommand};
use crate::theme::PRIMARY;

const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Upper frequency bound for input fields in MHz (KC901V tops at 6.8 GHz).
const MAX_MHZ: f64 = 6800.0;

/// Calibration choices in display order (no `Cal::ALL` in core).
const CALS: [Cal; 4] = [Cal::CalOn, Cal::CalOff, Cal::CalSys, Cal::CalUser];

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
            state.send(WorkerCommand::RunS11(state.s11.s11_params()));
        } else {
            state.status_message = Some(StatusMessage::Text(Text::RunForDisplay));
        }
    }
}

/// START/STOP/CENTER/SPAN frequency fields plus the POINTS count.
fn sweep_fields(ui: &mut egui::Ui, state: &mut AppState) {
    let mut start_stop_edited = false;
    let mut center_span_edited = false;

    egui::Grid::new("s11_sweep_grid")
        .num_columns(2)
        .spacing([8.0, 8.0])
        .show(ui, |ui| {
            let s11 = &mut state.s11;
            ui.label(state.language.text(Text::Start));
            start_stop_edited |= freq_field(ui, &mut s11.start_hz);
            ui.end_row();
            ui.label(state.language.text(Text::Stop));
            start_stop_edited |= freq_field(ui, &mut s11.stop_hz);
            ui.end_row();
            ui.label(state.language.text(Text::Center));
            center_span_edited |= freq_field(ui, &mut s11.center_hz);
            ui.end_row();
            ui.label(state.language.text(Text::Span));
            center_span_edited |= freq_field(ui, &mut s11.span_hz);
            ui.end_row();
            ui.label(state.language.text(Text::Points));
            ui.add(egui::DragValue::new(&mut s11.points).range(2..=10001));
            ui.end_row();
        });

    if start_stop_edited {
        state.s11.start_stop_changed();
    } else if center_span_edited {
        state.s11.center_span_changed();
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
                    for cal in CALS {
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
                        for value in Rbw::ALL {
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
    } else {
        let button =
            egui::Button::new(egui::RichText::new(state.language.text(Text::Run)).strong())
                .fill(PRIMARY);
        if ui.add_sized(size, button).clicked() {
            state.send(WorkerCommand::RunS11(state.s11.s11_params()));
            state.s11.running = true;
            state.s11.needs_fit = true;
            state.s11.view_locked = false;
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
