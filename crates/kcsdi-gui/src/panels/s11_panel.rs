// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Receiver controls for one trace definition.

use super::sweep_controls::{choice_button, group_heading};
use crate::i18n::{Language, Text};
use crate::state::DEVICE_MODEL;
use crate::workspace::TraceSettings;
use kcsdi_core::commands::Cal;

pub fn receiver_fields(ui: &mut egui::Ui, settings: &mut TraceSettings, language: Language) {
    group_heading(ui, language.text(Text::Calibration));
    let capabilities = DEVICE_MODEL.capabilities();
    let calibrations = if settings.display == crate::workspace::TraceDisplay::Spec {
        capabilities.spec_calibrations()
    } else {
        capabilities.s11_calibrations()
    };
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 8.0 * calibrations.len().saturating_sub(1) as f32)
            / calibrations.len() as f32;
        for &cal in calibrations {
            let text = language.text(match cal {
                Cal::CalOn => Text::CalOn,
                Cal::CalOff => Text::CalOff,
                Cal::CalSys => Text::CalSys,
                Cal::CalUser => Text::CalUser,
            });
            if choice_button(ui, text, settings.cal == cal, width) {
                settings.cal = cal;
            }
        }
    });
    group_heading(ui, language.text(Text::Rbw));
    for row in capabilities.rbw_list.chunks(4) {
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0 * row.len().saturating_sub(1) as f32)
                / row.len() as f32;
            for &rbw in row {
                if choice_button(ui, &rbw.to_string(), settings.rbw == rbw, width) {
                    settings.rbw = rbw;
                }
            }
        });
    }
}
