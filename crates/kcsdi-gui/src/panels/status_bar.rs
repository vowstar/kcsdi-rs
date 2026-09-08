// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Bottom status bar: transient messages and instrument health.

use crate::i18n::{StatusMessage, Text};
use crate::state::{AppState, ConnectionState, WorkerCommand};

/// Draw connection and health status. Return whether to open connection settings.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) -> bool {
    let mut open_connection = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let language = state.language;
        let endpoint = if state.host.trim().is_empty() {
            language.text(Text::Connection).to_owned()
        } else {
            format!("{}:{}", state.host, state.port)
        };
        if ui
            .button(egui::RichText::new(endpoint).monospace())
            .clicked()
        {
            open_connection = true;
        }
        match &state.connection {
            ConnectionState::Connected => {
                ui.colored_label(crate::theme::SUCCESS, language.text(Text::Connected));
                if state.any_running() && ui.small_button(language.text(Text::StopSweep)).clicked()
                {
                    state.spec.running = false;
                    state.s11.running = false;
                    state.send(WorkerCommand::StopSweep);
                }
                if ui.small_button(language.text(Text::Disconnect)).clicked() {
                    state.spec.running = false;
                    state.s11.running = false;
                    state.send(WorkerCommand::Disconnect);
                }
            }
            ConnectionState::Connecting => {
                ui.spinner();
                ui.label(language.text(Text::Connecting));
            }
            ConnectionState::Disconnected | ConnectionState::Error(_) => {
                if ui.button(language.text(Text::Connect)).clicked() {
                    if state.host.trim().is_empty() {
                        open_connection = true;
                    } else {
                        state.connection = ConnectionState::Connecting;
                        state.send(WorkerCommand::Connect {
                            host: state.host.clone(),
                            port: state.port,
                        });
                    }
                }
            }
        }
        if let Some(msg) = &state.status_message {
            let text = msg.text(language);
            if matches!(msg, StatusMessage::Detail(_)) {
                ui.colored_label(ui.visuals().error_fg_color, language.text(Text::Error))
                    .on_hover_text(text);
            } else if !matches!(
                msg,
                StatusMessage::Text(Text::Connected | Text::Disconnected)
            ) {
                ui.label(egui::RichText::new(text).small());
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if state.connection != ConnectionState::Connected {
                return;
            }
            let voltage = match &state.voltage {
                Some(v) => format!(
                    "{} {:.2} V / {} {:.2} V",
                    state.language.text(Text::ExternalPower),
                    v.external,
                    state.language.text(Text::Battery),
                    v.battery
                ),
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
    open_connection
}
