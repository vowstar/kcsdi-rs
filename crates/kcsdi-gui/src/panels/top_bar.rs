// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Top bar: connection controls and device identity summary.

use crate::state::{AppState, ConnectionState, WorkerCommand};
use crate::theme::PRIMARY;

const GREEN: egui::Color32 = egui::Color32::from_rgb(0x43, 0xa0, 0x47);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xff, 0x98, 0x00);
const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Draw the top bar. Signature is a module contract; do not change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // Host and port are only editable while disconnected.
        let editable = matches!(
            state.connection,
            ConnectionState::Disconnected | ConnectionState::Error(_)
        );
        ui.add_enabled_ui(editable, |ui| {
            ui.label("Host");
            ui.add(egui::TextEdit::singleline(&mut state.host).desired_width(110.0));
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut state.port).range(1..=65535));
        });

        ui.separator();

        match &state.connection {
            ConnectionState::Disconnected | ConnectionState::Error(_) => {
                let can_connect = !state.host.trim().is_empty();
                if ui
                    .add_enabled(can_connect, egui::Button::new("Connect"))
                    .clicked()
                {
                    let cmd = WorkerCommand::Connect {
                        host: state.host.clone(),
                        port: state.port,
                    };
                    state.connection = ConnectionState::Connecting;
                    state.send(cmd);
                }
            }
            ConnectionState::Connecting => {
                ui.add_enabled(false, egui::Button::new("Connect"));
                ui.add(egui::Spinner::new());
            }
            ConnectionState::Connected => {
                if ui.button("Disconnect").clicked() {
                    if state.spec.running {
                        state.spec.running = false;
                        state.send(WorkerCommand::StopSpec);
                    }
                    state.send(WorkerCommand::Disconnect);
                }
            }
        }

        let (color, text) = match &state.connection {
            ConnectionState::Disconnected => (egui::Color32::GRAY, "Disconnected"),
            ConnectionState::Connecting => (AMBER, "Connecting"),
            ConnectionState::Connected => (GREEN, "Connected"),
            ConnectionState::Error(_) => (RED, "Error"),
        };
        status_dot(ui, color);
        ui.label(text);

        // Device identity pinned to the right edge while connected.
        if state.connection == ConnectionState::Connected {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(info) = &state.device_info {
                    ui.label(
                        egui::RichText::new(format!("{}  sw {}", info.serial, info.software))
                            .monospace()
                            .color(PRIMARY),
                    );
                }
            });
        }
    });
}

/// Small colored circle used as the connection status indicator.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}
