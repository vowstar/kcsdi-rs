// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Local device profiles, desktop settings and existing device information.

use kcsdi_core::connection::ConnectionTarget;
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceProfile {
    pub name: String,
    #[serde(flatten)]
    pub target: ConnectionTarget,
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
    pub lookup: crate::device_lookup::DeviceLookup,
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
        if self.normalized().target.validate().is_err() {
            return Some(match self.target {
                ConnectionTarget::Tcp { .. } => Text::ProfileHostRequired,
                ConnectionTarget::Serial { .. } => Text::SerialPathRequired,
            });
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
            target: crate::connection_editor::normalized(&self.target),
        }
    }
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
            if state.target.validate().is_ok() {
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
    show_discovery(ui.ctx(), state);
}

fn show_devices(ui: &mut egui::Ui, state: &mut AppState) {
    let language = state.language;
    let button_rect = egui::Align2::CENTER_BOTTOM.align_size_within_rect(
        egui::vec2(152.0, 36.0),
        ui.max_rect().shrink2(egui::vec2(0.0, 16.0)),
    );
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(language.text(Text::LocalDevices)).strong());
        if ui.button(language.text(Text::DiscoverDevices)).clicked() {
            state.desktop.lookup.discovery_open = true;
        }
    });
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

fn show_discovery(ctx: &egui::Context, state: &mut AppState) {
    let language = state.language;
    let lookup = &mut state.desktop.lookup;
    let was_open = lookup.discovery_open;
    let mut add = None;
    egui::Window::new(language.text(Text::DiscoverDevices))
        .id(egui::Id::new("device_discovery"))
        .open(&mut lookup.discovery_open)
        .default_width(420.0)
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(language.text(Text::DiscoveryHelp));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !lookup.discovery.is_pending(),
                        egui::Button::new(language.text(Text::DiscoveryScan)),
                    )
                    .clicked()
                {
                    lookup.discovery.start_scan();
                }
                if lookup.discovery.is_pending() {
                    ui.spinner();
                    if ui.button(language.text(Text::Cancel)).clicked() {
                        lookup.discovery.cancel();
                    }
                }
            });
            if let Some(error) = &lookup.discovery.error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    language.text(Text::DiscoveryFailed),
                )
                .on_hover_text(error);
            }
            let devices = lookup
                .discovery
                .data
                .as_ref()
                .map(|snapshot| snapshot.devices.as_slice())
                .unwrap_or_default();
            if devices.is_empty() {
                ui.weak(language.text(Text::DiscoveryEmpty));
            }
            egui::ScrollArea::vertical()
                .max_height(250.0)
                .show(ui, |ui| {
                    for device in devices {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    device.is_fresh(std::time::Instant::now()),
                                    egui::Button::new(language.text(Text::AddDiscovered)),
                                )
                                .clicked()
                            {
                                add = Some(DeviceProfile {
                                    name: device.hostname.clone(),
                                    target: device.target.clone(),
                                });
                            }
                            let caption = format!("{}  {}", device.product, device.target);
                            ui.add(egui::Label::new(caption).truncate())
                                .on_hover_text(&device.hostname);
                        });
                    }
                });
        });
    if lookup.discovery_open {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
    if let Some(profile) = add {
        state.desktop.editor = Some(ProfileEditor {
            index: None,
            profile,
        });
        lookup.discovery_open = false;
    }
    if was_open && !lookup.discovery_open {
        lookup.discovery.cancel();
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
            let selected = state.target == profile.target;
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
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&profile.name).size(15.0).strong(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&profile.name);
                            ui.small(format!(
                                "{} / {}",
                                DEVICE_MODEL.name(),
                                crate::connection_editor::kind_label(&profile.target, language)
                            ));
                            ui.add_space(6.0);
                            let endpoint = profile.target.to_string();
                            ui.add(
                                egui::Label::new(egui::RichText::new(&endpoint).monospace())
                                    .truncate(),
                            )
                            .on_hover_text(endpoint);
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
    if busy && state.target != profile.target {
        return;
    }
    state.target = profile.target.clone();
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
                state.connection == ConnectionState::Connected
                    && !state.health.pending
                    && !state.calibration.busy()
                    && !state.source.busy(),
                egui::Button::new(language.text(Text::Refresh)),
            )
            .on_disabled_hover_text(language.text(if state.calibration.busy() {
                Text::CalibrationBusy
            } else if state.source.busy() {
                Text::SourceStopForHealth
            } else if state.health.pending {
                Text::HealthUpdating
            } else {
                Text::DeviceInfoUnavailable
            }))
            .clicked()
        {
            state.send(WorkerCommand::RefreshStatus);
        }
        if state.connection == ConnectionState::Connected && state.health.pending {
            ui.spinner()
                .on_hover_text(language.text(Text::HealthUpdating));
        }
    });
    let Some(info) = state
        .device_info
        .as_ref()
        .filter(|_| state.connection == ConnectionState::Connected)
    else {
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
            let now = std::time::Instant::now();
            let health = &state.health;
            let readings = health.snapshot.as_ref();
            for (key, value) in [
                (
                    Text::Temperature,
                    readings.map(|reading| format!("{:.1} C", reading.temperature)),
                ),
                (
                    Text::ExternalPower,
                    readings.map(|reading| format!("{:.2} V", reading.voltage.external)),
                ),
                (
                    Text::Battery,
                    readings.map(|reading| format!("{:.2} V", reading.voltage.battery)),
                ),
            ] {
                ui.label(language.text(key));
                let mut text =
                    egui::RichText::new(value.unwrap_or_else(|| "--".into())).monospace();
                if health.is_stale(now) || readings.is_none() {
                    text = text.weak();
                }
                ui.add(egui::Label::new(text).truncate()).on_hover_text(
                    crate::panels::status_bar::health_details(health, now, language),
                );
                ui.end_row();
            }
            ui.label(language.text(Text::HealthStatus));
            ui.add(
                egui::Label::new(crate::panels::status_bar::health_caption(
                    health, now, language,
                ))
                .truncate(),
            )
            .on_hover_text(crate::panels::status_bar::health_details(
                health, now, language,
            ));
            ui.end_row();
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
        ui.small(DEVICE_MODEL.name());
        ui.add_space(12.0);
        ui.label(language.text(Text::DeviceName));
        ui.add(egui::TextEdit::singleline(&mut editor.profile.name).desired_width(f32::INFINITY));
        crate::connection_editor::show(
            ui,
            &mut editor.profile.target,
            &mut state.desktop.lookup.ports,
            language,
        );
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
    if state.desktop.editor.is_none() {
        state.desktop.lookup.ports.cancel();
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

    fn health_about_state(case: usize, language: Language) -> AppState {
        let mut state = AppState {
            language,
            connection: match case {
                8 => ConnectionState::Disconnected,
                9 => ConnectionState::Connecting,
                _ => ConnectionState::Connected,
            },
            health: crate::panels::status_bar::tests::example_health(case),
            device_info: Some(kcsdi_core::data::DeviceInfo {
                username: "bench".into(),
                software: "V1.6.1".into(),
                hardware: "V1.0".into(),
                serial: "0000000001".into(),
                copyright: String::new(),
            }),
            ..Default::default()
        };
        state.desktop.page = Page::About;
        state
    }

    #[test]
    fn about_health_states_have_fixed_rows_and_fit_both_languages_and_sizes() {
        for language in Language::ALL {
            for theme_mode in [ThemeMode::Dark, ThemeMode::Light] {
                for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
                    for case in 0..10 {
                        let ctx = egui::Context::default();
                        theme::setup(&ctx);
                        theme::apply(&ctx, theme_mode);
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        let mut state = health_about_state(case, language);
                        for _ in 0..3 {
                            let output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| show_home(ui, &mut state),
                            );
                            let mut labels = Vec::new();
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape {
                                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                    assert!(
                                        screen.contains_rect(bounds),
                                        "{language:?} {theme_mode:?} case{case}: {} {bounds:?}",
                                        text.galley.text()
                                    );
                                    assert!(shape.clip_rect.contains_rect(bounds));
                                    labels.push((text.galley.text(), bounds));
                                }
                            }
                            if case < 8 {
                                for key in [
                                    Text::Temperature,
                                    Text::ExternalPower,
                                    Text::Battery,
                                    Text::HealthStatus,
                                ] {
                                    assert!(
                                        labels.iter().any(|(text, _)| *text == language.text(key))
                                    );
                                }
                                let blank_count =
                                    labels.iter().filter(|(text, _)| *text == "--").count();
                                assert_eq!(
                                    blank_count,
                                    if matches!(case, 0 | 1 | 7) { 3 } else { 0 }
                                );
                                if matches!(case, 4..=6) {
                                    assert!(labels.iter().any(|(text, _)| {
                                        text.contains(language.text(Text::HealthStale))
                                    }));
                                }
                            } else {
                                assert!(
                                    labels.iter().any(|(text, _)| *text
                                        == language.text(Text::DeviceInfoUnavailable))
                                );
                                assert!(!labels.iter().any(|(text, _)| text.contains("42.0 C")));
                            }
                            output.drop_without_applying_deltas();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn about_refresh_is_disabled_while_a_request_is_pending() {
        let ctx = egui::Context::default();
        let mut state = health_about_state(3, Language::English);
        let (sender, receiver) = std::sync::mpsc::channel();
        state.cmd_tx = Some(sender);
        let frame = |state: &mut AppState, events, time| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    events,
                    time: Some(time),
                    ..Default::default()
                },
                |ui| show_about(ui, state),
            )
        };
        let output = frame(&mut state, vec![], 0.0);
        let at = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Refresh" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap();
        output.drop_without_applying_deltas();
        for (round, pending) in [true, false].into_iter().enumerate() {
            state.health.pending = pending;
            for (index, events) in [
                vec![egui::Event::PointerMoved(at)],
                vec![egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }],
                vec![egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ]
            .into_iter()
            .enumerate()
            {
                frame(&mut state, events, (round * 4 + index + 1) as f64 * 0.02)
                    .drop_without_applying_deltas();
            }
            if pending {
                assert!(receiver.try_recv().is_err());
            } else {
                assert!(matches!(
                    receiver.try_recv().unwrap().command,
                    WorkerCommand::RefreshStatus
                ));
            }
        }
    }

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
                    target: ConnectionTarget::Tcp {
                        host: "first.example.invalid".into(),
                        port: 901,
                    },
                },
                DeviceProfile {
                    name: "Bench B".into(),
                    target: ConnectionTarget::Tcp {
                        host: "second.example.invalid".into(),
                        port: 901,
                    },
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
            target: ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 4321,
            },
        });
        let trace = kcsdi_core::data::SweepData {
            mode: kcsdi_core::protocol::StreamMode::S11,
            format: "loss".into(),
            points: Vec::new(),
        };
        state.workspace.selected_mut().unwrap().completed =
            Some(std::sync::Arc::new(crate::acquisition::CompletedSweep {
                segments: None,
                data: trace.clone(),
                settings: crate::acquisition::AcquisitionSettings::S11(
                    crate::acquisition::tests::s11(),
                ),
                session_id: 0,
                completed_at: std::time::SystemTime::UNIX_EPOCH,
            }));
        open_profile(&mut state, 0);
        assert_eq!(
            state.target,
            ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 4321
            }
        );
        assert_eq!(state.desktop.page, Page::Instrument);
        assert_eq!(state.connection, ConnectionState::Disconnected);
        assert_eq!(
            state
                .workspace
                .selected()
                .unwrap()
                .completed
                .as_ref()
                .unwrap()
                .data,
            trace
        );
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn unavailable_serial_profile_is_valid_and_open_never_connects() {
        let (tx, rx) = std::sync::mpsc::channel();
        let target = ConnectionTarget::Serial {
            path: "/dev/serial/by-id/not-present".into(),
        };
        let profile = DeviceProfile {
            name: "Serial bench".into(),
            target: target.clone(),
        };
        assert!(profile.validation_error(&[], None).is_none());
        let mut state = AppState {
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.desktop.settings.profiles.push(profile);
        open_profile(&mut state, 0);
        assert_eq!(state.target, target);
        assert_eq!(state.connection, ConnectionState::Disconnected);
        assert!(!state.desktop.lookup.is_pending());
        assert!(rx.try_recv().is_err());
        state.connection = ConnectionState::Connected;
        state.desktop.settings.profiles.push(DeviceProfile {
            name: "Other".into(),
            target: ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 901,
            },
        });
        open_profile(&mut state, 1);
        assert_eq!(state.target, target);
    }

    #[test]
    fn discovery_add_only_opens_an_editor_and_expired_results_cannot_be_added() {
        use kcsdi_core::discovery::{DiscoveredDevice, DiscoverySnapshot};
        let ctx = egui::Context::default();
        theme::setup(&ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = AppState {
            language: Language::English,
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.desktop.lookup.discovery_open = true;
        let target = ConnectionTarget::Tcp {
            host: "192.0.2.1".into(),
            port: 4321,
        };
        let now = std::time::Instant::now();
        state.desktop.lookup.discovery.data = Some(DiscoverySnapshot {
            devices: vec![DiscoveredDevice {
                target: target.clone(),
                hostname: "synthetic.local".into(),
                product: "KC901V".into(),
                observed_at: now,
                expires_at: now + std::time::Duration::from_secs(30),
            }],
        });
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 600.0),
            )),
            ..Default::default()
        };
        let mut at = None;
        for _ in 0..3 {
            let output = ctx.run_ui(input(), |ui| show_home(ui, &mut state));
            at = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Add" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            });
            output.drop_without_applying_deltas();
        }
        let at = at.unwrap();
        for pressed in [true, false] {
            let mut event = input();
            event.events = vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ];
            ctx.run_ui(event, |ui| show_home(ui, &mut state))
                .drop_without_applying_deltas();
        }
        assert_eq!(
            state.desktop.editor.as_ref().unwrap().profile.target,
            target
        );
        assert!(state.desktop.settings.profiles.is_empty());
        assert_eq!(state.target, ConnectionTarget::default());
        assert_eq!(state.session_id, 0);
        assert_eq!(state.request_id, 0);
        assert!(rx.try_recv().is_err());
        state.desktop.editor = None;
        state.desktop.lookup.discovery_open = true;
        state
            .desktop
            .lookup
            .discovery
            .data
            .as_mut()
            .unwrap()
            .devices[0]
            .expires_at = now;
        state.desktop.lookup.poll();
        let output = ctx.run_ui(input(), |ui| show_home(ui, &mut state));
        assert!(!output.shapes.iter().any(
            |shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "Add")
        ));
        output.drop_without_applying_deltas();
    }

    #[test]
    fn connection_editors_and_discovery_fit_languages_themes_and_window_sizes() {
        use kcsdi_core::transport::serial::SerialPortInfo;
        for language in Language::ALL {
            for theme_mode in [ThemeMode::Light, ThemeMode::Dark] {
                for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
                    for serial in [false, true] {
                        let ctx = egui::Context::default();
                        theme::setup(&ctx);
                        theme::apply(&ctx, theme_mode);
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        let mut state = AppState {
                            language,
                            ..Default::default()
                        };
                        state.desktop.editor = Some(ProfileEditor {
                            index: None,
                            profile: DeviceProfile {
                                name: "Bench".into(),
                                target: if serial {
                                    ConnectionTarget::Serial {
                                        path: format!("/dev/serial/by-id/{}", "x".repeat(220)),
                                    }
                                } else {
                                    ConnectionTarget::Tcp {
                                        host: "instrument.local".into(),
                                        port: 901,
                                    }
                                },
                            },
                        });
                        state.desktop.lookup.ports.data = Some(vec![SerialPortInfo {
                            path: "COM7".into(),
                            label: format!("COM7 {}", "USB metadata ".repeat(30)),
                        }]);
                        for _ in 0..3 {
                            let output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| show_home(ui, &mut state),
                            );
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape {
                                    let visible = text
                                        .galley
                                        .rect
                                        .translate(text.pos.to_vec2())
                                        .intersect(shape.clip_rect);
                                    if visible.is_positive() {
                                        assert!(
                                            screen.contains_rect(visible),
                                            "{language:?} {theme_mode:?}: {} {visible:?}",
                                            text.galley.text()
                                        );
                                    }
                                    if [
                                        language.text(Text::Save),
                                        language.text(Text::Cancel),
                                        language.text(Text::Serial),
                                        language.text(Text::SerialPath),
                                    ]
                                    .contains(&text.galley.text())
                                    {
                                        assert!(shape.clip_rect.contains_rect(
                                            text.galley.rect.translate(text.pos.to_vec2())
                                        ));
                                    }
                                }
                            }
                            output.drop_without_applying_deltas();
                        }
                        assert!(!state.desktop.lookup.is_pending());
                        state.desktop.editor = None;
                        state.desktop.lookup.discovery_open = true;
                        for _ in 0..3 {
                            let output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| show_home(ui, &mut state),
                            );
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape {
                                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                                    assert!(
                                        screen.contains_rect(rect),
                                        "{language:?}: {} {rect:?}",
                                        text.galley.text()
                                    );
                                }
                            }
                            output.drop_without_applying_deltas();
                        }
                        assert!(!state.desktop.lookup.is_pending());
                    }
                }
            }
        }
    }

    #[test]
    fn an_active_session_cannot_be_retargeted_by_opening_another_profile() {
        let mut state = AppState {
            target: ConnectionTarget::Tcp {
                host: "current.local".into(),
                port: 901,
            },
            connection: ConnectionState::Connected,
            ..AppState::default()
        };
        state.desktop.settings.profiles.push(DeviceProfile {
            name: "Other".into(),
            target: ConnectionTarget::Tcp {
                host: "other.local".into(),
                port: 901,
            },
        });
        open_profile(&mut state, 0);
        assert_eq!(
            state.target,
            ConnectionTarget::Tcp {
                host: "current.local".into(),
                port: 901
            }
        );
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
            assert!(
                ConnectionTarget::Tcp {
                    host: host.into(),
                    port: 901
                }
                .validate()
                .is_ok(),
                "{host}"
            );
        }
        for host in [
            "",
            "https://instrument.local",
            "host:901",
            "bad host",
            "a..local",
            "-name.local",
        ] {
            assert!(
                ConnectionTarget::Tcp {
                    host: host.into(),
                    port: 901
                }
                .validate()
                .is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn editing_a_profile_keeps_its_name_but_cannot_take_another_profiles_name() {
        let profiles = vec![DeviceProfile {
            name: "Bench".into(),
            target: ConnectionTarget::Tcp {
                host: "instrument.local".into(),
                port: 901,
            },
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
