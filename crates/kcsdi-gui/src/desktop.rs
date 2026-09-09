// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Local device profiles, desktop settings and existing device information.

use serde::{Deserialize, Serialize};

use crate::i18n::Text;
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, WorkerCommand};
use crate::theme::{self, ThemeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Devices,
    Settings,
    About,
    Instrument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceProfile {
    pub name: String,
    pub host: String,
    pub port: u16,
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 901,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub profiles: Vec<DeviceProfile>,
    pub theme: ThemeMode,
}

#[derive(Debug, Default)]
pub struct DesktopState {
    pub page: Page,
    pub settings: DesktopConfig,
    editor: Option<ProfileEditor>,
    delete_profile: Option<usize>,
}

#[derive(Debug)]
struct ProfileEditor {
    index: Option<usize>,
    profile: DeviceProfile,
}

impl DeviceProfile {
    fn validation_error(&self, profiles: &[Self], own_index: Option<usize>) -> Option<Text> {
        let name = self.name.trim();
        if name.is_empty() {
            return Some(Text::ProfileNameRequired);
        }
        if !valid_host(self.host.trim()) || self.port == 0 {
            return Some(Text::ProfileHostRequired);
        }
        if profiles.iter().enumerate().any(|(index, profile)| {
            Some(index) != own_index && profile.name.trim().to_lowercase() == name.to_lowercase()
        }) {
            return Some(Text::ProfileNameExists);
        }
        None
    }

    fn normalized(&self) -> Self {
        Self {
            name: self.name.trim().to_owned(),
            host: self.host.trim().to_owned(),
            port: self.port,
        }
    }
}

fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Show the local desktop shell. Opening a profile only selects its target.
pub fn show_home(ui: &mut egui::Ui, state: &mut AppState) {
    egui::Panel::left("desktop_navigation")
        .exact_size(128.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .inner_margin(8),
        )
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (page, text) in [
                (Page::Devices, Text::Devices),
                (Page::Settings, Text::Settings),
                (Page::About, Text::About),
            ] {
                if ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new(state.language.text(text))
                            .selected(state.desktop.page == page),
                    )
                    .clicked()
                {
                    state.desktop.page = page;
                }
            }
            if !state.host.is_empty() {
                ui.add_space(12.0);
                ui.separator();
                if ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new(state.language.text(Text::Instrument)),
                    )
                    .clicked()
                {
                    state.desktop.page = Page::Instrument;
                }
            }
        });
    egui::CentralPanel::default().show(ui, |ui| {
        if state.desktop.page == Page::Devices {
            show_devices(ui, state);
        } else {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(8.0);
                match state.desktop.page {
                    Page::Settings => show_settings(ui, state),
                    Page::About => show_about(ui, state),
                    Page::Devices | Page::Instrument => {}
                }
            });
        }
    });
    show_profile_editor(ui.ctx(), state);
    show_delete_confirmation(ui.ctx(), state);
}

fn show_devices(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    let button_rect = egui::Align2::CENTER_BOTTOM.align_size_within_rect(
        egui::vec2(152.0, 36.0),
        ui.max_rect().shrink2(egui::vec2(0.0, 16.0)),
    );
    ui.label(egui::RichText::new(language.text(Text::LocalDevices)).strong());
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .max_height((ui.available_height() - 64.0).max(0.0))
        .show(ui, |ui| {
            show_profiles(ui, state);
        });
    if ui
        .put(
            button_rect,
            egui::Button::new(
                egui::RichText::new(language.text(Text::AddDevice)).color(egui::Color32::WHITE),
            )
            .fill(ui.visuals().hyperlink_color),
        )
        .clicked()
    {
        state.desktop.editor = Some(ProfileEditor {
            index: None,
            profile: DeviceProfile::default(),
        });
    }
}

fn show_profiles(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    if state.desktop.settings.profiles.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(28.0);
            ui.weak(language.text(Text::NoSavedDevices));
        });
        return;
    }
    let mut open = None;
    let mut edit = None;
    let mut delete = None;
    let busy = matches!(
        state.connection,
        ConnectionState::Connected | ConnectionState::Connecting | ConnectionState::Disconnecting
    );
    ui.horizontal_wrapped(|ui| {
        for (index, profile) in state.desktop.settings.profiles.iter().enumerate() {
            let selected = state.host == profile.host && state.port == profile.port;
            let stroke = if selected {
                egui::Stroke::new(1.0, ui.visuals().hyperlink_color)
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            };
            egui::Frame::new()
                .fill(ui.visuals().window_fill)
                .stroke(stroke)
                .corner_radius(4)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(216.0);
                        ui.set_min_height(132.0);
                        ui.push_id(index, |ui| {
                            ui.label(egui::RichText::new(&profile.name).size(15.0).strong());
                            ui.small(format!("{} / TCP", DEVICE_MODEL.name()));
                            ui.add_space(6.0);
                            ui.monospace(format!("{}:{}", profile.host, profile.port));
                            let status = if selected {
                                match state.connection {
                                    ConnectionState::Connected => Text::Connected,
                                    ConnectionState::Connecting => Text::Connecting,
                                    ConnectionState::Disconnecting => Text::Disconnecting,
                                    ConnectionState::Error(_) => Text::Error,
                                    ConnectionState::Disconnected => Text::Disconnected,
                                }
                            } else {
                                Text::Disconnected
                            };
                            ui.weak(language.text(status));
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let response = ui.add_enabled(
                                    !busy || selected,
                                    egui::Button::new(language.text(Text::OpenDevice)),
                                );
                                if response.clicked() {
                                    open = Some(index);
                                }
                                if busy && !selected {
                                    response
                                        .on_disabled_hover_text(language.text(Text::SingleSession));
                                }
                                if ui.button(language.text(Text::Edit)).clicked() {
                                    edit = Some(index);
                                }
                                if ui.button(language.text(Text::Delete)).clicked() {
                                    delete = Some(index);
                                }
                            });
                        });
                    });
                });
        }
    });
    if let Some(index) = open {
        open_profile(state, index);
    }
    if let Some(index) = edit {
        state.desktop.editor = Some(ProfileEditor {
            index: Some(index),
            profile: state.desktop.settings.profiles[index].clone(),
        });
    }
    state.desktop.delete_profile = delete.or(state.desktop.delete_profile);
}

fn open_profile(state: &mut AppState, index: usize) {
    let Some(profile) = state.desktop.settings.profiles.get(index) else {
        return;
    };
    let busy = matches!(
        state.connection,
        ConnectionState::Connected | ConnectionState::Connecting | ConnectionState::Disconnecting
    );
    if busy && (state.host != profile.host || state.port != profile.port) {
        return;
    }
    state.host = profile.host.clone();
    state.port = profile.port;
    state.desktop.page = Page::Instrument;
}

fn show_settings(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    ui.heading(language.text(Text::Settings));
    ui.add_space(20.0);
    ui.label(egui::RichText::new(language.text(Text::Appearance)).strong());
    ui.add_space(8.0);
    egui::Grid::new("desktop_appearance")
        .spacing([28.0, 16.0])
        .show(ui, |ui| {
            ui.label(language.text(Text::Language));
            let mut preference = state.language_preference;
            egui::ComboBox::from_id_salt("desktop_language")
                .selected_text(preference.label(language))
                .show_ui(ui, |ui| {
                    for option in crate::i18n::LanguagePreference::ALL {
                        ui.selectable_value(&mut preference, option, option.label(language));
                    }
                });
            if preference != state.language_preference {
                state.set_language_preference(preference);
                crate::i18n::set_language(ui.ctx(), state.language);
                ui.ctx().request_repaint();
            }
            ui.end_row();
            ui.label(language.text(Text::Theme));
            ui.horizontal(|ui| {
                for option in ThemeMode::ALL {
                    if ui
                        .radio_value(
                            &mut state.desktop.settings.theme,
                            option,
                            option.label(language),
                        )
                        .changed()
                    {
                        theme::apply(ui.ctx(), state.desktop.settings.theme);
                    }
                }
            });
            ui.end_row();
        });
}

fn show_about(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    ui.heading(language.text(Text::About));
    ui.add_space(18.0);
    ui.label(egui::RichText::new("kcsdi-rs").size(24.0).strong());
    ui.label(language.text(Text::AppDescription));
    ui.horizontal(|ui| {
        ui.label(language.text(Text::AppVersion));
        ui.monospace(env!("CARGO_PKG_VERSION"));
    });
    ui.hyperlink_to("GitHub", env!("CARGO_PKG_REPOSITORY"));
    ui.add_space(24.0);
    ui.separator();
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(language.text(Text::DeviceDetails)).strong());
        if ui
            .add_enabled(
                state.connection == ConnectionState::Connected,
                egui::Button::new(language.text(Text::Refresh)),
            )
            .clicked()
        {
            state.send(WorkerCommand::RefreshStatus);
        }
    });
    let Some(info) = &state.device_info else {
        ui.add_space(8.0);
        ui.weak(language.text(Text::DeviceInfoUnavailable));
        return;
    };
    egui::Grid::new("desktop_device_information")
        .spacing([24.0, 9.0])
        .show(ui, |ui| {
            for (key, value) in [
                (Text::SerialNumber, info.serial.as_str()),
                (Text::SoftwareVersion, info.software.as_str()),
                (Text::HardwareVersion, info.hardware.as_str()),
                (Text::DeviceUser, info.username.as_str()),
            ] {
                ui.label(language.text(key));
                ui.monospace(value);
                ui.end_row();
            }
            if let Some(temperature) = state.temperature {
                ui.label(language.text(Text::Temperature));
                ui.monospace(format!("{temperature:.1} C"));
                ui.end_row();
            }
            if let Some(voltage) = &state.voltage {
                for (key, value) in [
                    (Text::ExternalPower, voltage.external),
                    (Text::Battery, voltage.battery),
                ] {
                    ui.label(language.text(key));
                    ui.monospace(format!("{value:.2} V"));
                    ui.end_row();
                }
            }
        });
}

fn show_profile_editor(ctx: &egui::Context, state: &mut AppState) {
    let Some(mut editor) = state.desktop.editor.take() else {
        return;
    };
    let language = state.language;
    let mut save = false;
    let mut cancel = false;
    let response = egui::Modal::new(egui::Id::new("profile_editor")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading(language.text(if editor.index.is_some() {
            Text::EditDevice
        } else {
            Text::AddDevice
        }));
        ui.small(format!("{} / TCP", DEVICE_MODEL.name()));
        ui.add_space(12.0);
        ui.label(language.text(Text::DeviceName));
        ui.add(egui::TextEdit::singleline(&mut editor.profile.name).desired_width(f32::INFINITY));
        ui.label(language.text(Text::Host));
        ui.add(egui::TextEdit::singleline(&mut editor.profile.host).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            ui.label(language.text(Text::Port));
            ui.add(egui::DragValue::new(&mut editor.profile.port).range(1..=65535));
        });
        let error = editor
            .profile
            .validation_error(&state.desktop.settings.profiles, editor.index);
        if let Some(error) = error {
            ui.weak(language.text(error));
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            save = ui
                .add_enabled(
                    error.is_none(),
                    egui::Button::new(language.text(Text::Save)),
                )
                .clicked();
            cancel = ui.button(language.text(Text::Cancel)).clicked();
        });
    });
    if save {
        let profile = editor.profile.normalized();
        match editor.index {
            Some(index) => state.desktop.settings.profiles[index] = profile,
            None => state.desktop.settings.profiles.push(profile),
        }
    } else if !cancel && !response.should_close() {
        state.desktop.editor = Some(editor);
    }
}

fn show_delete_confirmation(ctx: &egui::Context, state: &mut AppState) {
    let Some(index) = state.desktop.delete_profile else {
        return;
    };
    let Some(profile) = state.desktop.settings.profiles.get(index) else {
        state.desktop.delete_profile = None;
        return;
    };
    let language = state.language;
    let mut delete = false;
    let mut cancel = false;
    let response = egui::Modal::new(egui::Id::new("delete_profile")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading(language.text(Text::DeleteDevice));
        ui.label(&profile.name);
        ui.label(language.text(Text::DeleteDeviceHelp));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            cancel = ui.button(language.text(Text::Cancel)).clicked();
            delete = ui
                .button(
                    egui::RichText::new(language.text(Text::Delete))
                        .color(ui.visuals().error_fg_color),
                )
                .clicked();
        });
    });
    if delete {
        state.desktop.settings.profiles.remove(index);
    }
    if delete || cancel || response.should_close() {
        state.desktop.delete_profile = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    #[test]
    fn profile_cards_stack_labels_and_keep_neighboring_cards_separate() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            theme::setup(&ctx);
            theme::apply(&ctx, ThemeMode::Dark);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 850.0));
            let mut state = AppState {
                language,
                ..AppState::default()
            };
            state.desktop.settings.profiles = vec![
                DeviceProfile {
                    name: "Bench A".into(),
                    host: "first.example.invalid".into(),
                    port: 901,
                },
                DeviceProfile {
                    name: "Bench B".into(),
                    host: "second.example.invalid".into(),
                    port: 901,
                },
            ];
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ui| {
                        show_home(ui, &mut state);
                    },
                );
                let labels: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) => Some((
                            text.galley.job.text.clone(),
                            text.galley.rect.translate(text.pos.to_vec2()),
                        )),
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                let find = |text: &str, occurrence: usize| {
                    labels
                        .iter()
                        .filter(|(label, _)| label == text)
                        .nth(occurrence)
                        .map(|(_, rect)| *rect)
                        .unwrap_or_else(|| panic!("missing {text}"))
                };
                let first_title = find("Bench A", 0);
                let second_title = find("Bench B", 0);
                // 216 px content, two 12 px margins, two 1 px borders and an 8 px gap.
                assert!((second_title.left() - first_title.left() - 250.0).abs() < 1.0);
                assert!((first_title.top() - second_title.top()).abs() < 1.0);
                for (index, host, title) in [
                    (0, "first.example.invalid:901", first_title),
                    (1, "second.example.invalid:901", second_title),
                ] {
                    let model = find("KC901V / TCP", index);
                    let host = find(host, 0);
                    let status = find(language.text(Text::Disconnected), index);
                    let action = find(language.text(Text::OpenDevice), index);
                    for pair in [title, model, host, status, action].windows(2) {
                        assert!(pair[0].bottom() < pair[1].top(), "{language:?}: {pair:?}");
                    }
                    for rect in [title, model, host, status, action] {
                        assert!(screen.contains_rect(rect));
                        assert!(
                            rect.left() >= title.left() && rect.right() <= title.left() + 216.0
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn opening_a_profile_selects_the_target_without_connecting_or_clearing_traces() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..AppState::default()
        };
        state.desktop.settings.profiles.push(DeviceProfile {
            name: "Bench".into(),
            host: "instrument.local".into(),
            port: 4321,
        });
        let trace = kcsdi_core::data::SweepData {
            mode: kcsdi_core::protocol::StreamMode::S11,
            format: "loss".into(),
            points: Vec::new(),
        };
        state.s11.trace = Some(trace.clone());
        open_profile(&mut state, 0);
        assert_eq!(state.host, "instrument.local");
        assert_eq!(state.port, 4321);
        assert_eq!(state.desktop.page, Page::Instrument);
        assert_eq!(state.connection, ConnectionState::Disconnected);
        assert_eq!(state.s11.trace, Some(trace));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn an_active_session_cannot_be_retargeted_by_opening_another_profile() {
        let mut state = AppState {
            host: "current.local".into(),
            connection: ConnectionState::Connected,
            ..AppState::default()
        };
        state.desktop.settings.profiles.push(DeviceProfile {
            name: "Other".into(),
            host: "other.local".into(),
            port: 901,
        });
        open_profile(&mut state, 0);
        assert_eq!(state.host, "current.local");
        assert_eq!(state.desktop.page, Page::Devices);
    }

    #[test]
    fn profiles_accept_connection_targets_and_reject_urls_and_empty_labels() {
        for host in [
            "127.0.0.1",
            "::1",
            "instrument.local",
            "analyzer.example.invalid.",
        ] {
            assert!(valid_host(host), "{host}");
        }
        for host in [
            "",
            "https://instrument.local",
            "host:901",
            "bad host",
            "a..local",
            "-name.local",
        ] {
            assert!(!valid_host(host), "{host}");
        }
    }

    #[test]
    fn editing_a_profile_keeps_its_name_but_cannot_take_another_profiles_name() {
        let profiles = vec![DeviceProfile {
            name: "Bench".into(),
            host: "instrument.local".into(),
            port: 901,
        }];
        let profile = DeviceProfile {
            name: " bench ".into(),
            ..profiles[0].clone()
        };
        assert_eq!(
            profile.validation_error(&profiles, None),
            Some(Text::ProfileNameExists)
        );
        assert_eq!(profile.validation_error(&profiles, Some(0)), None);
        assert_eq!(profile.normalized().name, "bench");
    }
}
