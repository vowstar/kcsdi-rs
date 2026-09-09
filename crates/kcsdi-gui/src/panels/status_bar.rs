// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Bottom status bar: transient messages and instrument health.

use std::time::{Duration, Instant};

use crate::health::HealthState;
use crate::i18n::{Language, StatusMessage, Text};
use crate::state::{AppState, ConnectionState, SweepState, WorkerCommand};

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
                    state.send(WorkerCommand::StopSweep);
                }
                if ui.small_button(language.text(Text::Disconnect)).clicked() {
                    state.send(WorkerCommand::Disconnect);
                }
                if state.sweep == SweepState::Stopping {
                    ui.spinner();
                    ui.label(language.text(Text::Stopping));
                } else if let Some(preview) = state
                    .workspace
                    .traces
                    .iter()
                    .filter_map(|trace| trace.preview.as_deref())
                    .max_by_key(|preview| preview.cycle_id)
                {
                    ui.label(format!(
                        "{} {}/{}",
                        language.text(Text::SweepProgress),
                        preview.data.points.len(),
                        preview.group.settings.points()
                    ));
                }
            }
            ConnectionState::Connecting => {
                ui.spinner();
                ui.label(language.text(Text::Connecting));
            }
            ConnectionState::Disconnecting => {
                ui.spinner();
                ui.label(language.text(if state.worker_shutdown.is_cancelled() {
                    Text::Closing
                } else {
                    Text::Disconnecting
                }));
            }
            ConnectionState::Disconnected | ConnectionState::Error(_) => {
                if ui.button(language.text(Text::Connect)).clicked() {
                    if state.host.trim().is_empty() {
                        open_connection = true;
                    } else {
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
            let now = Instant::now();
            let health = &state.health;
            let caption = health_caption(health, now, language);
            let details = health_details(health, now, language);
            ui.label(egui::RichText::new(caption).small())
                .on_hover_text(&details);
            if health.pending {
                ui.spinner()
                    .on_hover_text(language.text(Text::HealthUpdating));
            }
            if let Some(snapshot) = &health.snapshot {
                let values = format!(
                    "{:.1} C  {} {:.2} V / {} {:.2} V",
                    snapshot.temperature,
                    language.text(Text::ExternalPower),
                    snapshot.voltage.external,
                    language.text(Text::Battery),
                    snapshot.voltage.battery,
                );
                let mut text = egui::RichText::new(&values).monospace();
                if health.is_stale(now) {
                    text = text.weak();
                }
                ui.add(egui::Label::new(text).truncate())
                    .on_hover_text(format!("{values}\n{details}"));
            }
        });
    });
    open_connection
}

fn age_text(age: Duration, language: Language) -> String {
    let seconds = age.as_secs();
    let (amount, unit) = if seconds < 60 {
        (seconds, Text::HealthSecondsAgo)
    } else if seconds < 3600 {
        (seconds / 60, Text::HealthMinutesAgo)
    } else if seconds < 86400 {
        (seconds / 3600, Text::HealthHoursAgo)
    } else {
        (seconds / 86400, Text::HealthDaysAgo)
    };
    format!("{amount} {}", language.text(unit))
}

/// Use the same freshness language in the compact bar and device details.
pub(crate) fn health_caption(health: &HealthState, now: Instant, language: Language) -> String {
    if let Some(age) = health.age(now) {
        let age = age_text(age, language);
        if health.is_stale(now) {
            format!("{}, {age}", language.text(Text::HealthStale))
        } else {
            age
        }
    } else {
        language
            .text(if health.pending {
                Text::HealthUpdating
            } else if health.error.is_some() {
                Text::HealthRefreshFailed
            } else {
                Text::HealthUnavailable
            })
            .into()
    }
}

pub(crate) fn health_details(health: &HealthState, now: Instant, language: Language) -> String {
    let mut details = health_caption(health, now, language);
    if health.pending && health.snapshot.is_some() {
        details.push_str(&format!("\n{}", language.text(Text::HealthUpdating)));
    }
    if let Some(error) = &health.error {
        details.push_str(&format!(
            "\n{}: {error}",
            language.text(Text::HealthRefreshFailed)
        ));
    }
    details
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::theme::{self, ThemeMode};

    pub(crate) fn example_health(case: usize) -> HealthState {
        let now = Instant::now();
        let mut health = HealthState::default();
        if case >= 2 && case != 7 {
            let observed = if matches!(case, 4 | 5) {
                now - Duration::from_secs(40)
            } else {
                now
            };
            health.succeed(crate::health::tests::snapshot(42.0, observed), now);
        }
        if matches!(case, 1 | 3 | 5) {
            health.begin();
        }
        if matches!(case, 6 | 7) {
            health.fail("err_par5 status query failure ".repeat(50), now);
        }
        health
    }

    #[test]
    fn age_units_and_state_words_preserve_staleness_during_refresh() {
        let now = Instant::now();
        for language in Language::ALL {
            for (seconds, expected) in [
                (0, Text::HealthSecondsAgo),
                (59, Text::HealthSecondsAgo),
                (60, Text::HealthMinutesAgo),
                (3599, Text::HealthMinutesAgo),
                (3600, Text::HealthHoursAgo),
                (86400, Text::HealthDaysAgo),
            ] {
                assert!(
                    age_text(Duration::from_secs(seconds), language)
                        .ends_with(language.text(expected))
                );
            }
            let mut health = HealthState::default();
            assert_eq!(
                health_caption(&health, now, language),
                language.text(Text::HealthUnavailable)
            );
            health.begin();
            assert_eq!(
                health_caption(&health, now, language),
                language.text(Text::HealthUpdating)
            );
            health.succeed(crate::health::tests::snapshot(42.0, now), now);
            assert_eq!(
                health_caption(&health, now + Duration::from_secs(5), language),
                format!("5 {}", language.text(Text::HealthSecondsAgo))
            );
            health.begin();
            let stale = health_caption(&health, now + Duration::from_secs(35), language);
            assert!(stale.contains(language.text(Text::HealthStale)));
            assert!(stale.contains("35"));
            assert!(
                health_details(&health, now + Duration::from_secs(35), language)
                    .contains(language.text(Text::HealthUpdating))
            );
            health.fail("err_par5".into(), now);
            assert!(
                health_caption(&health, now, language).contains(language.text(Text::HealthStale))
            );
            assert!(health_details(&health, now, language).contains("err_par5"));
            health.succeed(crate::health::tests::snapshot(43.0, now), now);
            assert!(!health_details(&health, now, language).contains("err_par5"));
        }
    }

    #[test]
    fn health_bar_states_fit_both_window_sizes_languages_and_themes() {
        for language in Language::ALL {
            for theme_mode in [ThemeMode::Dark, ThemeMode::Light] {
                for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
                    for case in 0..10 {
                        let ctx = egui::Context::default();
                        theme::setup(&ctx);
                        theme::apply(&ctx, theme_mode);
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        let mut state = AppState {
                            language,
                            host: "loopback.example.invalid".into(),
                            port: 19117,
                            connection: match case {
                                8 => ConnectionState::Disconnected,
                                9 => ConnectionState::Connecting,
                                _ => ConnectionState::Connected,
                            },
                            health: example_health(case),
                            sweep: if case < 8 {
                                SweepState::Running
                            } else {
                                SweepState::Idle
                            },
                            ..Default::default()
                        };
                        for _ in 0..3 {
                            let output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| {
                                    egui::Panel::bottom("test_status").show(ui, |ui| {
                                        show(ui, &mut state);
                                    });
                                },
                            );
                            let mut texts = Vec::new();
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape {
                                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                    assert!(
                                        screen.contains_rect(bounds),
                                        "{language:?} {theme_mode:?} case{case}: {} {bounds:?}",
                                        text.galley.text()
                                    );
                                    assert!(shape.clip_rect.contains_rect(bounds));
                                    texts.push(text.galley.text());
                                }
                            }
                            assert_eq!(
                                texts.iter().any(|text| text.contains("42.0 C")),
                                (2..8).contains(&case) && case != 7
                            );
                            if matches!(case, 4..=6) {
                                assert!(texts.iter().any(|text| text.contains(language.text(Text::HealthStale))));
                            }
                            if case == 0 {
                                assert!(texts.contains(&language.text(Text::HealthUnavailable)));
                            }
                            if case == 1 {
                                assert!(texts.contains(&language.text(Text::HealthUpdating)));
                            }
                            if case == 7 {
                                assert!(texts.contains(&language.text(Text::HealthRefreshFailed)));
                            }
                            output.drop_without_applying_deltas();
                        }
                    }
                }
            }
        }
    }
}
