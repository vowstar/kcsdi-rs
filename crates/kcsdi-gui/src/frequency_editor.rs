// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Frequency drafts and owned background file dialogs.

use std::thread::JoinHandle;

use kcsdi_core::control::CancellationToken;
use kcsdi_core::model::FreqRange;

use crate::frequency_list::{self, FrequencyUnit};
use crate::i18n::{Language, Text};

enum FileResult {
    Imported(Vec<u64>),
    Saved,
}

type FileTask = JoinHandle<Result<Option<FileResult>, String>>;

pub struct FrequencyEditor {
    open: bool,
    text: String,
    unit: FrequencyUnit,
    error: Option<String>,
    worker: Option<FileTask>,
    cancel: CancellationToken,
}

impl Default for FrequencyEditor {
    fn default() -> Self {
        Self {
            open: false,
            text: String::new(),
            unit: FrequencyUnit::Hz,
            error: None,
            worker: None,
            cancel: CancellationToken::default(),
        }
    }
}

impl FrequencyEditor {
    pub fn open(&mut self, list: &[u64]) {
        if self.is_pending() {
            return;
        }
        self.open = true;
        self.replace(list);
        self.error = None;
    }

    fn replace(&mut self, list: &[u64]) {
        self.unit = FrequencyUnit::Hz;
        self.text = list
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("\n");
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
        let result = self.worker.take().expect("finished file task").join();
        if self.cancel.is_cancelled() {
            return;
        }
        match result {
            Ok(Ok(Some(FileResult::Imported(list)))) => {
                self.replace(&list);
                self.error = None;
            }
            Ok(Ok(_)) => self.error = None,
            Ok(Err(error)) => self.error = Some(error),
            Err(_) => self.error = Some("frequency file worker failed".into()),
        }
    }

    fn file_dialog(
        &mut self,
        ctx: &egui::Context,
        language: Language,
        import: bool,
        allowed: FreqRange,
    ) {
        if self.is_pending() {
            return;
        }
        self.cancel = CancellationToken::default();
        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        let unit = self.unit;
        let task = std::thread::Builder::new()
            .name("frequency-file".into())
            .spawn(move || {
                let result = (|| {
                    let dialog = rfd::FileDialog::new().set_title(language.text(if import {
                        Text::ImportFrequencyList
                    } else {
                        Text::FrequencyTemplate
                    }));
                    let path = if import {
                        dialog
                            .add_filter("Frequency list", &["txt", "csv", "xlsx"])
                            .pick_file()
                    } else {
                        dialog
                            .add_filter("XLSX", &["xlsx"])
                            .set_file_name("frequencies.xlsx")
                            .save_file()
                    };
                    let Some(mut path) = path.filter(|_| !cancel.is_cancelled()) else {
                        return Ok(None);
                    };
                    if import {
                        frequency_list::read_file(&path, unit)
                            .and_then(|list| checked_range(list, allowed))
                            .map(|list| Some(FileResult::Imported(list)))
                    } else {
                        if path.extension().is_none() {
                            path.set_extension("xlsx");
                        }
                        if !path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("xlsx"))
                        {
                            return Err("the frequency template requires an .xlsx extension".into());
                        }
                        let bytes = frequency_list::template_bytes()?;
                        if cancel.is_cancelled() {
                            return Ok(None);
                        }
                        kcsdi_core::atomic_file::write(&path, &bytes, false)
                            .map_err(|error| error.to_string())?;
                        Ok(Some(FileResult::Saved))
                    }
                })();
                ctx.request_repaint();
                result
            });
        match task {
            Ok(worker) => {
                self.worker = Some(worker);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        language: Language,
        allowed: FreqRange,
    ) -> Option<Vec<u64>> {
        if !self.open {
            return None;
        }
        let mut applied = None;
        egui::Modal::new(egui::Id::new("frequency_editor")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(language.text(Text::FrequencyList));
            ui.label(language.text(Text::FrequencyListHelp));
            ui.small(format!(
                "{} {} {} Hz",
                allowed.min_hz,
                language.text(Text::RangeTo),
                allowed.max_hz
            ));
            let pending = self.is_pending();
            ui.add_enabled_ui(!pending, |ui| {
                ui.horizontal(|ui| {
                    ui.label(language.text(Text::Unit));
                    egui::ComboBox::from_id_salt("frequency_list_unit")
                        .selected_text(self.unit.label())
                        .show_ui(ui, |ui| {
                            for unit in FrequencyUnit::ALL {
                                ui.selectable_value(&mut self.unit, unit, unit.label());
                            }
                        });
                    if ui
                        .button(language.text(Text::ImportFrequencyList))
                        .clicked()
                    {
                        self.file_dialog(ctx, language, true, allowed);
                    }
                    if ui.button(language.text(Text::FrequencyTemplate)).clicked() {
                        self.file_dialog(ctx, language, false, allowed);
                    }
                });
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        if ui
                            .add(
                                egui::TextEdit::multiline(&mut self.text)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(10)
                                    .char_limit(64 * 1024),
                            )
                            .changed()
                        {
                            self.error = None;
                        }
                    });
                let parsed = frequency_list::parse_text(&self.text, self.unit)
                    .and_then(|list| checked_range(list, allowed));
                if let Some(error) = self.error.as_ref().or(parsed.as_ref().err()) {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            parsed.is_ok() && !self.is_pending(),
                            egui::Button::new(language.text(Text::Apply)),
                        )
                        .clicked()
                    {
                        applied = parsed.ok();
                        self.open = false;
                    }
                    if ui.button(language.text(Text::Cancel)).clicked() {
                        self.open = false;
                    }
                });
            });
            if self.is_pending() {
                ui.spinner();
                ui.label(language.text(Text::FrequencyFilePending));
            }
        });
        applied
    }
}

fn checked_range(list: Vec<u64>, allowed: FreqRange) -> Result<Vec<u64>, String> {
    if let Some(value) = list
        .iter()
        .find(|&&value| value < allowed.min_hz || value > allowed.max_hz)
    {
        Err(format!(
            "{value} Hz is outside {} to {} Hz",
            allowed.min_hz, allowed.max_hz
        ))
    } else {
        Ok(list)
    }
}

impl Drop for FrequencyEditor {
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
    fn failed_or_cancelled_import_preserves_the_draft() {
        let mut editor = FrequencyEditor::default();
        editor.open(&[5_000, 6_000, 7_000]);
        let original = editor.text.clone();
        editor.worker = Some(std::thread::spawn(|| Err("invalid row".into())));
        while !editor.worker.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        editor.poll();
        assert_eq!(editor.text, original);
        assert_eq!(editor.error.as_deref(), Some("invalid row"));
        editor.worker = Some(std::thread::spawn(|| {
            Ok(Some(FileResult::Imported(vec![1, 2, 3])))
        }));
        editor.cancel();
        while !editor.worker.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        editor.poll();
        assert_eq!(editor.text, original);
        assert!(!editor.open);
        assert!(!editor.is_pending());
    }

    #[test]
    fn point_boundaries_do_not_inherit_a_finite_span() {
        let range = FreqRange::new(5_000, 7_000_000_000, 1_000);
        assert_eq!(
            checked_range(vec![5_000; 3], range).unwrap(),
            vec![5_000; 3]
        );
        assert!(checked_range(vec![0, 5_000, 6_000], range).is_err());
    }

    #[test]
    fn list_editor_fits_both_languages_at_the_minimum_window_size() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 600.0));
            let mut editor = FrequencyEditor::default();
            editor.open(&[5_000, 6_000, 7_000]);
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ui| {
                        assert!(
                            editor
                                .show(
                                    ui.ctx(),
                                    language,
                                    FreqRange::new(5_000, 7_000_000_000, 1_000)
                                )
                                .is_none()
                        );
                    },
                );
                for shape in &output.shapes {
                    if let egui::Shape::Text(text) = &shape.shape
                        && [
                            Text::FrequencyList,
                            Text::ImportFrequencyList,
                            Text::FrequencyTemplate,
                            Text::Apply,
                            Text::Cancel,
                        ]
                        .iter()
                        .any(|&key| text.galley.job.text == language.text(key))
                    {
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        assert!(screen.contains_rect(bounds), "{:?}: {:?}", language, bounds);
                    }
                }
                output.drop_without_applying_deltas();
            }
        }
    }
}
