// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Top bar: connection controls and device identity summary.

use crate::i18n::Text;
use crate::state::{AppState, ConnectionState, WorkerCommand};
use crate::theme::PRIMARY;

const GREEN: egui::Color32 = egui::Color32::from_rgb(0x43, 0xa0, 0x47);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xff, 0x98, 0x00);
const RED: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Draw the top bar. Signature is a module contract; do not change it.
pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    ui.set_width(360.0);
    let editable = matches!(
        state.connection,
        ConnectionState::Disconnected | ConnectionState::Error(_)
    );
    ui.add_enabled_ui(editable, |ui| {
        crate::connection_editor::show(
            ui,
            &mut state.target,
            &mut state.desktop.lookup.ports,
            language,
        );
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        match &state.connection {
            ConnectionState::Disconnected | ConnectionState::Error(_) => {
                let target = crate::connection_editor::normalized(&state.target);
                let can_connect = target.validate().is_ok();
                if ui
                    .add_enabled(can_connect, egui::Button::new(language.text(Text::Connect)))
                    .clicked()
                {
                    state.target = target.clone();
                    state.send(WorkerCommand::Connect { target });
                }
            }
            ConnectionState::Connecting => {
                ui.add_enabled(false, egui::Button::new(language.text(Text::Connect)));
                ui.add(egui::Spinner::new());
            }
            ConnectionState::Connected => {
                if ui.button(language.text(Text::Disconnect)).clicked() {
                    state.send(WorkerCommand::Disconnect);
                }
            }
            ConnectionState::Disconnecting => {
                ui.add_enabled(false, egui::Button::new(language.text(Text::Disconnecting)));
                ui.spinner();
            }
        }

        let (color, text) = match &state.connection {
            ConnectionState::Disconnected => {
                (egui::Color32::GRAY, language.text(Text::Disconnected))
            }
            ConnectionState::Connecting => (AMBER, language.text(Text::Connecting)),
            ConnectionState::Connected => (GREEN, language.text(Text::Connected)),
            ConnectionState::Disconnecting => (AMBER, language.text(Text::Disconnecting)),
            ConnectionState::Error(_) => (RED, language.text(Text::Error)),
        };
        status_dot(ui, color);
        ui.label(text);
    });
    if state.connection == ConnectionState::Connected
        && let Some(info) = &state.device_info
    {
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "{}  {} {}",
                    info.serial,
                    language.text(Text::Firmware),
                    info.software
                ))
                .monospace()
                .color(PRIMARY),
            )
            .truncate(),
        );
    }
}

/// Small colored circle used as the connection status indicator.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}
