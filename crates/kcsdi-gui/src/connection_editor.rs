// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Shared explicit connection fields. Rendering never enumerates ports.

use kcsdi_core::connection::ConnectionTarget;
use kcsdi_core::transport::serial::SerialPortInfo;

use crate::device_lookup::Lookup;
use crate::i18n::{Language, Text};

pub fn kind_label(target: &ConnectionTarget, language: Language) -> &'static str {
    language.text(match target {
        ConnectionTarget::Tcp { .. } => Text::Network,
        ConnectionTarget::Serial { .. } => Text::Serial,
    })
}

pub fn normalized(target: &ConnectionTarget) -> ConnectionTarget {
    match target {
        ConnectionTarget::Tcp { host, port } => ConnectionTarget::Tcp {
            host: host.trim().into(),
            port: *port,
        },
        // A manual path is an OS identifier, not a shell expression.
        ConnectionTarget::Serial { path } => ConnectionTarget::Serial { path: path.clone() },
    }
}

pub fn show(
    ui: &mut egui::Ui,
    target: &mut ConnectionTarget,
    ports: &mut Lookup<Vec<SerialPortInfo>>,
    language: Language,
) {
    let mut serial = matches!(target, ConnectionTarget::Serial { .. });
    let before = serial;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut serial, false, language.text(Text::Network));
        ui.selectable_value(&mut serial, true, language.text(Text::Serial));
    });
    if before != serial {
        ports.cancel();
        *target = if serial {
            ConnectionTarget::Serial {
                path: String::new(),
            }
        } else {
            ConnectionTarget::default()
        };
    }
    match target {
        ConnectionTarget::Tcp { host, port } => {
            ui.label(language.text(Text::Host));
            ui.add(egui::TextEdit::singleline(host).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.label(language.text(Text::Port));
                ui.add(egui::DragValue::new(port).range(1..=65535));
            });
        }
        ConnectionTarget::Serial { path } => {
            ui.label(language.text(Text::SerialPath));
            ui.add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !ports.is_pending(),
                        egui::Button::new(language.text(Text::Refresh)),
                    )
                    .clicked()
                {
                    ports.refresh_ports();
                }
                if ports.is_pending() {
                    ui.spinner();
                    if ui.button(language.text(Text::Cancel)).clicked() {
                        ports.cancel();
                    }
                }
                ui.weak("921600 / 8N1");
            });
            if let Some(error) = &ports.error {
                ui.add(egui::Label::new(language.text(Text::PortLookupFailed)).truncate())
                    .on_hover_text(error);
            }
            if let Some(entries) = &ports.data {
                if entries.is_empty() {
                    ui.weak(language.text(Text::NoSerialPorts));
                }
                egui::ScrollArea::vertical()
                    .id_salt("serial_choices")
                    .max_height(96.0)
                    .show(ui, |ui| {
                        for entry in entries {
                            if ui
                                .add(
                                    egui::Button::new(&entry.label)
                                        .selected(*path == entry.path)
                                        .truncate(),
                                )
                                .on_hover_text(&entry.path)
                                .clicked()
                            {
                                *path = entry.path.clone();
                            }
                        }
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_results_never_replaces_a_manual_path_but_a_click_does() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut target = ConnectionTarget::Serial {
            path: "/dev/manual-unavailable".into(),
        };
        let mut ports = Lookup::default();
        ports.data = Some(vec![SerialPortInfo {
            path: "COM7".into(),
            label: "USB fixture".into(),
        }]);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 600.0),
            )),
            ..Default::default()
        };
        let mut at = None;
        for _ in 0..3 {
            let output = ctx.run_ui(input(), |ui| {
                egui::Window::new("Connection test").show(ui.ctx(), |ui| {
                    ui.set_width(360.0);
                    show(ui, &mut target, &mut ports, Language::English);
                });
            });
            at = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "USB fixture" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            });
            output.drop_without_applying_deltas();
        }
        assert_eq!(
            target,
            ConnectionTarget::Serial {
                path: "/dev/manual-unavailable".into()
            }
        );
        assert!(!ports.is_pending());
        let at = at.unwrap();
        for pressed in [true, false] {
            let mut input = input();
            input.events = vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ];
            ctx.run_ui(input, |ui| {
                egui::Window::new("Connection test").show(ui.ctx(), |ui| {
                    ui.set_width(360.0);
                    show(ui, &mut target, &mut ports, Language::English);
                });
            })
            .drop_without_applying_deltas();
        }
        assert_eq!(
            target,
            ConnectionTarget::Serial {
                path: "COM7".into()
            }
        );
        ports.data = Some(Vec::new());
        ports.error = Some("synthetic enumeration failure".into());
        ctx.run_ui(input(), |ui| {
            show(ui, &mut target, &mut ports, Language::English)
        })
        .drop_without_applying_deltas();
        assert_eq!(
            target,
            ConnectionTarget::Serial {
                path: "COM7".into()
            }
        );
    }
}
