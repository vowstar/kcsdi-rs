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
    if state.any_running() && sweep_controls::invalid_step(ui.ctx(), "workspace") {
        state.send(WorkerCommand::StopSweep);
    }
}

pub fn run_button(ui: &mut egui::Ui, state: &mut AppState) {
    ui.add_enabled_ui(state.connection == ConnectionState::Connected, |ui| {
        let size = [ui.available_width(), BUTTON_HEIGHT];
        if state.sweep == SweepState::Stopping {
            ui.add_enabled_ui(false, |ui| {
                ui.add_sized(size, egui::Button::new(state.language.text(Text::Stopping)));
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
            let plan = if sweep_controls::invalid_step(ui.ctx(), "workspace") {
                Err(kcsdi_core::Error::InvalidParameter(
                    state.language.text(Text::InvalidStep).into(),
                ))
            } else {
                state.workspace.plan()
            };
            if let Some(plan) = sweep_controls::run_button(ui, plan, state.language) {
                state.send(WorkerCommand::RunWorkspace(plan));
            }
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
