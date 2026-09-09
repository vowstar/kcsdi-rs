// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Run definitions and an isolated draft with an owned directory dialog.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kcsdi_core::control::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::i18n::{Language, Text};

pub const MAX_INTERVAL_MS: u64 = 365 * 24 * 60 * 60 * 1_000;
pub const MAX_RETAINED_FILES: u32 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalUnit {
    Minutes,
    Hours,
    Days,
    #[default]
    #[serde(other)]
    Seconds,
}

impl IntervalUnit {
    const ALL: [Self; 4] = [Self::Seconds, Self::Minutes, Self::Hours, Self::Days];

    fn milliseconds(self) -> u64 {
        match self {
            Self::Seconds => 1_000,
            Self::Minutes => 60_000,
            Self::Hours => 3_600_000,
            Self::Days => 86_400_000,
        }
    }

    fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Seconds => Text::RunSeconds,
            Self::Minutes => Text::RunMinutes,
            Self::Hours => Text::RunHours,
            Self::Days => Text::RunDays,
        })
    }

    fn readout(self, milliseconds: u64) -> String {
        let precision = match self {
            Self::Seconds => 3,
            Self::Minutes => 5,
            Self::Hours => 7,
            Self::Days => 9,
        };
        format!(
            "{:.precision$}",
            milliseconds as f64 / self.milliseconds() as f64
        )
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    Csv,
    #[default]
    #[serde(other)]
    Xlsx,
}

impl RecordingFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Xlsx => "xlsx",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    KeepLast(u32),
    #[default]
    #[serde(other)]
    KeepAll,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    pub enabled: bool,
    pub directory: PathBuf,
    pub format: RecordingFormat,
    pub retention: Retention,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunSettings {
    pub interval_ms: u64,
    pub interval_unit: IntervalUnit,
    pub recording: RecordingSettings,
}

impl RunSettings {
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validation_error()
            .map_or(Ok(()), |error| Err(Language::English.text(error).into()))
    }

    fn validation_error(&self) -> Option<Text> {
        if self.interval_ms > MAX_INTERVAL_MS {
            return Some(Text::RunIntervalInvalid);
        }
        if let Retention::KeepLast(count) = self.recording.retention
            && !(1..=MAX_RETAINED_FILES).contains(&count)
        {
            return Some(Text::RunRetentionInvalid);
        }
        if self.recording.enabled && !self.recording.directory.is_absolute() {
            return Some(Text::RunDirectoryRequired);
        }
        None
    }
}

#[derive(Clone, Debug)]
pub enum RunProgress {
    Acquiring,
    Saving,
    Waiting { until: Instant },
    Saved { path: PathBuf, pass_id: u64 },
}

type DirectoryTask = JoinHandle<Result<Option<PathBuf>, String>>;

pub struct RunSettingsEditor {
    open: bool,
    draft: RunSettings,
    interval_text: String,
    error: Option<String>,
    worker: Option<DirectoryTask>,
    cancel: CancellationToken,
}

impl Default for RunSettingsEditor {
    fn default() -> Self {
        Self {
            open: false,
            draft: RunSettings::default(),
            interval_text: "0".into(),
            error: None,
            worker: None,
            cancel: CancellationToken::default(),
        }
    }
}

impl RunSettingsEditor {
    pub fn open(&mut self, settings: &RunSettings) {
        if self.is_pending() {
            return;
        }
        self.open = true;
        self.draft = settings.clone();
        self.interval_text = settings.interval_unit.readout(settings.interval_ms);
        self.error = None;
    }

    pub fn is_pending(&self) -> bool {
        self.worker.is_some()
    }

    pub fn cancel(&mut self) {
        self.cancel.cancel();
        self.open = false;
    }

    pub fn poll(&mut self) {
        if !self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let result = self.worker.take().expect("finished directory task").join();
        if self.cancel.is_cancelled() {
            return;
        }
        match result {
            Ok(Ok(Some(path))) => {
                self.draft.recording.directory = path;
                self.error = None;
            }
            Ok(Ok(None)) => self.error = None,
            Ok(Err(error)) => self.error = Some(error),
            Err(_) => self.error = Some("directory dialog worker failed".into()),
        }
    }

    fn pick_directory(&mut self, ctx: &egui::Context, language: Language) {
        if self.is_pending() {
            return;
        }
        self.cancel = CancellationToken::default();
        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        let directory = self.draft.recording.directory.clone();
        match std::thread::Builder::new()
            .name("recording-directory".into())
            .spawn(move || {
                let mut dialog =
                    rfd::FileDialog::new().set_title(language.text(Text::RunDirectory));
                if directory.is_absolute() {
                    dialog = dialog.set_directory(directory);
                }
                let result = dialog.pick_folder().filter(|_| !cancel.is_cancelled());
                ctx.request_repaint();
                Ok(result)
            }) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn checked_draft(&self) -> Result<RunSettings, Text> {
        let mut draft = self.draft.clone();
        draft.interval_ms = parse_interval(&self.interval_text, draft.interval_unit)?;
        if let Some(error) = draft.validation_error() {
            return Err(error);
        }
        Ok(draft)
    }

    fn interval_fields(&mut self, ui: &mut egui::Ui, language: Language) {
        ui.horizontal(|ui| {
            ui.label(language.text(Text::RunInterval));
            ui.add(
                egui::TextEdit::singleline(&mut self.interval_text)
                    .desired_width(100.0)
                    .char_limit(32),
            );
            let previous = self.draft.interval_unit;
            for unit in IntervalUnit::ALL {
                ui.selectable_value(&mut self.draft.interval_unit, unit, unit.label(language));
            }
            if self.draft.interval_unit != previous
                && let Ok(milliseconds) = parse_interval(&self.interval_text, previous)
            {
                self.interval_text = self.draft.interval_unit.readout(milliseconds);
            }
        });
        ui.small(language.text(Text::RunIntervalHelp));
    }

    fn recording_fields(&mut self, ui: &mut egui::Ui, language: Language) {
        ui.checkbox(
            &mut self.draft.recording.enabled,
            language.text(Text::RunRecording),
        );
        ui.add_enabled_ui(self.draft.recording.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(language.text(Text::ExportFormat));
                ui.selectable_value(
                    &mut self.draft.recording.format,
                    RecordingFormat::Csv,
                    "CSV",
                );
                ui.selectable_value(
                    &mut self.draft.recording.format,
                    RecordingFormat::Xlsx,
                    "XLSX",
                );
            });
            ui.horizontal(|ui| {
                if ui.button(language.text(Text::RunChooseDirectory)).clicked() {
                    self.pick_directory(ui.ctx(), language);
                }
                let path = if self.draft.recording.directory.as_os_str().is_empty() {
                    language.text(Text::RunNoDirectory).to_owned()
                } else {
                    self.draft.recording.directory.display().to_string()
                };
                ui.add(egui::Label::new(&path).truncate())
                    .on_hover_text(path);
            });
            ui.horizontal(|ui| {
                ui.label(language.text(Text::RunRetention));
                let last = matches!(self.draft.recording.retention, Retention::KeepLast(_));
                let mut keep_last = last;
                ui.selectable_value(&mut keep_last, false, language.text(Text::RunKeepAll));
                ui.selectable_value(&mut keep_last, true, language.text(Text::RunKeepLast));
                if keep_last != last {
                    self.draft.recording.retention = if keep_last {
                        Retention::KeepLast(MAX_RETAINED_FILES)
                    } else {
                        Retention::KeepAll
                    };
                }
                if let Retention::KeepLast(count) = &mut self.draft.recording.retention {
                    ui.add(egui::DragValue::new(count).range(1..=MAX_RETAINED_FILES));
                    ui.label(language.text(Text::RunFiles));
                }
            });
            ui.small(language.text(Text::RunRetentionHelp));
            if matches!(self.draft.recording.retention, Retention::KeepLast(_)) {
                ui.small(language.text(Text::RunPruneHelp));
            }
        });
        if self.draft.recording.enabled
            && parse_interval(&self.interval_text, self.draft.interval_unit)
                .is_ok_and(|milliseconds| milliseconds < 10_000)
        {
            ui.colored_label(crate::theme::WARN, language.text(Text::RunShortInterval));
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, language: Language) -> Option<RunSettings> {
        if !self.open {
            return None;
        }
        let mut applied = None;
        egui::Modal::new(egui::Id::new("run_settings_editor")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(language.text(Text::RunSettings));
            let pending = self.is_pending();
            ui.add_enabled_ui(!pending, |ui| {
                self.interval_fields(ui, language);
                ui.separator();
                self.recording_fields(ui, language);
            });
            let checked = self.checked_draft();
            if let Some(error) = self
                .error
                .as_deref()
                .or_else(|| checked.as_ref().err().map(|&error| language.text(error)))
            {
                ui.add(
                    egui::Label::new(egui::RichText::new(error).color(ui.visuals().error_fg_color))
                        .truncate(),
                )
                .on_hover_text(error);
            }
            if self.is_pending() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(language.text(Text::RunFilePending));
                });
            }
            ui.separator();
            ui.small(language.text(Text::RunRestartHelp));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        checked.is_ok() && !self.is_pending(),
                        egui::Button::new(language.text(Text::Apply)),
                    )
                    .clicked()
                {
                    applied = checked.ok();
                    self.open = false;
                }
                if ui.button(language.text(Text::Cancel)).clicked() {
                    self.cancel();
                }
            });
        });
        applied
    }
}

fn parse_interval(text: &str, unit: IntervalUnit) -> Result<u64, Text> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| Text::RunIntervalInvalid)?;
    let milliseconds = value * unit.milliseconds() as f64;
    if !milliseconds.is_finite() || !(0.0..=MAX_INTERVAL_MS as f64).contains(&milliseconds) {
        return Err(Text::RunIntervalInvalid);
    }
    Ok(milliseconds.round() as u64)
}

impl Drop for RunSettingsEditor {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_config_do_not_start_recording() {
        let settings: RunSettings = toml::from_str("").unwrap();
        assert_eq!(settings, RunSettings::default());
        assert_eq!(settings.interval(), Duration::ZERO);
        assert_eq!(settings.recording.format, RecordingFormat::Xlsx);
        assert_eq!(settings.recording.retention, Retention::KeepAll);
        assert!(settings.validate().is_ok());
        let unknown: RunSettings = toml::from_str(
            "interval_unit = 'unknown'\n[recording]\nformat = 'unknown'\nretention = 'unknown'",
        )
        .unwrap();
        assert_eq!(unknown, settings);
        let partial: RunSettings = toml::from_str("interval_ms = 50\n[recording]").unwrap();
        assert_eq!(partial.interval(), Duration::from_millis(50));
        assert!(!partial.recording.enabled);
    }

    #[test]
    fn validation_rejects_unbounded_intervals_paths_and_retention() {
        let mut settings = RunSettings {
            interval_ms: MAX_INTERVAL_MS,
            ..Default::default()
        };
        assert!(settings.validate().is_ok());
        settings.interval_ms += 1;
        assert!(settings.validate().is_err());
        settings.interval_ms = 0;
        settings.recording.enabled = true;
        for path in ["", "recordings", "./recordings"] {
            settings.recording.directory = path.into();
            assert!(settings.validate().is_err());
        }
        settings.recording.directory = std::env::temp_dir();
        for count in [1, MAX_RETAINED_FILES] {
            settings.recording.retention = Retention::KeepLast(count);
            assert!(settings.validate().is_ok());
        }
        for count in [0, MAX_RETAINED_FILES + 1, u32::MAX] {
            settings.recording.retention = Retention::KeepLast(count);
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn interval_units_preserve_milliseconds_and_reject_nonfinite_values() {
        for unit in IntervalUnit::ALL {
            for milliseconds in [
                0,
                1,
                17,
                999,
                1_001,
                3_600_001,
                MAX_INTERVAL_MS - 1,
                MAX_INTERVAL_MS,
            ] {
                assert_eq!(
                    parse_interval(&unit.readout(milliseconds), unit),
                    Ok(milliseconds)
                );
            }
            for invalid in ["", "NaN", "inf", "-inf", "1e1000", "-0.01", "text"] {
                assert!(parse_interval(invalid, unit).is_err());
            }
        }
        assert_eq!(parse_interval("1.5", IntervalUnit::Minutes), Ok(90_000));
        assert!(parse_interval("365.01", IntervalUnit::Days).is_err());
    }

    #[test]
    fn edits_are_isolated_and_cancel_reopens_original_settings() {
        let source = RunSettings::default();
        let mut editor = RunSettingsEditor::default();
        editor.open(&source);
        editor.interval_text = "1.5".into();
        editor.draft.interval_unit = IntervalUnit::Minutes;
        assert_eq!(editor.checked_draft().unwrap().interval_ms, 90_000);
        assert_eq!(source.interval_ms, 0);
        editor.interval_text = "NaN".into();
        assert!(editor.checked_draft().is_err());
        editor.cancel();
        editor.open(&source);
        assert_eq!(editor.checked_draft().unwrap(), source);
    }

    #[test]
    fn cancelled_or_failed_directory_dialog_preserves_draft() {
        let source = RunSettings::default();
        let mut editor = RunSettingsEditor::default();
        editor.open(&source);
        editor.worker = Some(std::thread::spawn(|| Err("folder unavailable".into())));
        while !editor.worker.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        editor.poll();
        assert_eq!(editor.error.as_deref(), Some("folder unavailable"));
        assert_eq!(editor.draft, source);
        editor.worker = Some(std::thread::spawn(|| Ok(Some(std::env::temp_dir()))));
        editor.cancel();
        editor.open(&RunSettings {
            interval_ms: 99,
            ..Default::default()
        });
        assert!(!editor.open);
        while !editor.worker.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        editor.poll();
        assert_eq!(editor.draft, source);
        assert!(!editor.is_pending());
    }

    #[test]
    fn drop_joins_owned_directory_task() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut editor = RunSettingsEditor::default();
        editor.worker = Some(std::thread::spawn(move || {
            rx.recv().unwrap();
            Ok(None)
        }));
        let closer = std::thread::spawn(move || drop(editor));
        assert!(!closer.is_finished());
        tx.send(()).unwrap();
        closer.join().unwrap();
    }

    fn click_choice(
        ctx: &egui::Context,
        editor: &mut RunSettingsEditor,
        language: Language,
        caption: &str,
        time: &mut f64,
    ) -> Option<RunSettings> {
        let mut pointer = egui::Pos2::ZERO;
        let mut applied = None;
        for frame in 0..6 {
            *time += 1.0 / 30.0;
            let events = if frame == 3 || frame == 4 {
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed: frame == 3,
                        modifiers: Default::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    time: Some(*time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    if let Some(settings) = editor.show(ui.ctx(), language) {
                        applied = Some(settings);
                    }
                },
            );
            let target = (frame == 2).then(|| {
                output.shapes.iter().find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.job.text == caption
                    {
                        Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                    } else {
                        None
                    }
                })
            });
            output.drop_without_applying_deltas();
            assert!(!egui::Popup::is_any_open(ctx));
            if let Some(target) = target {
                pointer = target.expect("visible run setting choice");
            }
        }
        applied
    }

    #[test]
    fn inline_choices_update_only_the_draft_until_apply() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let settings = RunSettings {
                interval_ms: 60_001,
                recording: RecordingSettings {
                    enabled: true,
                    directory: std::env::temp_dir(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut editor = RunSettingsEditor::default();
            editor.open(&settings);
            let mut time = 0.0;
            for unit in IntervalUnit::ALL {
                assert!(
                    click_choice(&ctx, &mut editor, language, unit.label(language), &mut time)
                        .is_none()
                );
                assert_eq!(editor.draft.interval_unit, unit);
                assert_eq!(editor.checked_draft().unwrap().interval_ms, 60_001);
            }
            assert!(click_choice(&ctx, &mut editor, language, "CSV", &mut time).is_none());
            assert_eq!(editor.draft.recording.format, RecordingFormat::Csv);
            assert!(click_choice(&ctx, &mut editor, language, "XLSX", &mut time).is_none());
            assert_eq!(editor.draft.recording.format, RecordingFormat::Xlsx);
            assert!(
                click_choice(
                    &ctx,
                    &mut editor,
                    language,
                    language.text(Text::RunKeepLast),
                    &mut time
                )
                .is_none()
            );
            assert_eq!(
                editor.draft.recording.retention,
                Retention::KeepLast(MAX_RETAINED_FILES)
            );
            assert!(
                click_choice(
                    &ctx,
                    &mut editor,
                    language,
                    language.text(Text::RunKeepAll),
                    &mut time
                )
                .is_none()
            );
            assert_eq!(editor.draft.recording.retention, Retention::KeepAll);
            assert_eq!(settings.interval_unit, IntervalUnit::Seconds);
            let applied = click_choice(
                &ctx,
                &mut editor,
                language,
                language.text(Text::Apply),
                &mut time,
            )
            .unwrap();
            assert_eq!(applied.interval_unit, IntervalUnit::Days);
            assert_eq!(applied.interval_ms, settings.interval_ms);
            assert_eq!(applied.recording, settings.recording);
        }
    }

    #[test]
    fn recording_dialog_fits_both_languages_themes_and_window_sizes() {
        for language in Language::ALL {
            for theme in [
                crate::theme::ThemeMode::Light,
                crate::theme::ThemeMode::Dark,
            ] {
                for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
                    let ctx = egui::Context::default();
                    crate::theme::setup(&ctx);
                    crate::theme::apply(&ctx, theme);
                    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                    let settings = RunSettings {
                        recording: RecordingSettings {
                            enabled: true,
                            directory: std::env::temp_dir()
                                .join("long-recording-directory-name".repeat(20)),
                            retention: Retention::KeepLast(MAX_RETAINED_FILES),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let mut editor = RunSettingsEditor::default();
                    editor.open(&settings);
                    for _ in 0..3 {
                        let output = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |ui| {
                                assert!(editor.show(ui.ctx(), language).is_none());
                            },
                        );
                        for shape in &output.shapes {
                            if let egui::Shape::Text(text) = &shape.shape {
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    screen.contains_rect(bounds),
                                    "{language:?} {theme:?}: {:?} {bounds:?}",
                                    text.galley.job.text
                                );
                            }
                        }
                        output.drop_without_applying_deltas();
                    }
                }
            }
        }
    }
}
