// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Bottom status bar: transient messages and instrument health.

use crate::state::AppState;

/// Draw the status bar. Signature is a module contract; do not change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        if let Some(msg) = &state.status_message {
            ui.label(msg);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let voltage = match &state.voltage {
                Some(v) => format!("ext {:.2} V / bat {:.2} V", v.external, v.battery),
                None => "--".to_string(),
            };
            let temperature = match state.temperature {
                Some(t) => format!("{t:.1} C"),
                None => "--".to_string(),
            };
            ui.label(egui::RichText::new(voltage).monospace());
            ui.separator();
            ui.label(egui::RichText::new(temperature).monospace());
        });
    });
}
