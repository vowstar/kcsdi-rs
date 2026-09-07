// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Snapshot export on a separate thread, independent of the device worker.

use std::path::PathBuf;
use std::sync::mpsc;

use kcsdi_core::touchstone::{self, Document, Version};

use crate::i18n::{Language, Text};
use crate::state::S11State;

#[derive(Default)]
pub struct ExportState {
    version: Version,
    pending: Option<mpsc::Receiver<Outcome>>,
    outcome: Option<Outcome>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Saved(PathBuf),
    Cancelled,
    Failed(String),
}

impl ExportState {
    pub fn poll(&mut self) {
        let Some(receiver) = &self.pending else {
            return;
        };
        let outcome = match receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Outcome::Failed("export worker stopped".into())
            }
        };
        self.pending = None;
        self.outcome = Some(outcome);
    }

    fn start(&mut self, document: Document, language: Language, ctx: egui::Context) {
        let (sender, receiver) = mpsc::channel();
        self.pending = Some(receiver);
        self.outcome = None;
        let result = std::thread::Builder::new()
            .name("touchstone-export".into())
            .spawn(move || {
                let outcome = choose_and_save(document, language).unwrap_or_else(Outcome::Failed);
                let _ = sender.send(outcome);
                ctx.request_repaint();
            });
        if let Err(error) = result {
            self.pending = None;
            self.outcome = Some(Outcome::Failed(error.to_string()));
        }
    }
}

pub fn show(ui: &mut egui::Ui, export: &mut ExportState, s11: &S11State, language: Language) {
    let validation = s11.trace.as_ref().map(touchstone::validate_s1p);
    let ready = matches!(validation, Some(Ok(())));
    let busy = export.pending.is_some();
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("touchstone_version")
                .selected_text(format!("Touchstone {}", export.version.as_str()))
                .show_ui(ui, |ui| {
                    for version in [Version::V2, Version::V1] {
                        ui.selectable_value(&mut export.version, version, version.as_str());
                    }
                });
        });
    });
    let button = ui
        .add_enabled(
            !busy && ready,
            egui::Button::new(language.text(Text::ExportS1p)),
        )
        .on_hover_text(language.text(Text::ExportHelp));
    if button.clicked() {
        match snapshot(s11, export.version) {
            Ok(document) => export.start(document, language, ui.ctx().clone()),
            Err(error) => export.outcome = Some(Outcome::Failed(error)),
        }
    }
    if let Some(trace) = &s11.trace {
        let label = ui.small(format!(
            "{}: {} ({})",
            language.text(Text::ExportSnapshot),
            trace.points.len(),
            trace.format
        ));
        if let (Some(first), Some(last)) = (trace.points.first(), trace.points.last()) {
            label.on_hover_text(format!("{} .. {} Hz", first.freq_hz, last.freq_hz));
        }
    }
    if !ready {
        let label = ui.small(language.text(Text::ExportNeedsComplex));
        if let Some(Err(error)) = validation {
            label.on_hover_text(error.to_string());
        }
    }
    if busy {
        ui.small(language.text(Text::ExportBusy));
        // Also detect a worker panic, which cannot request its own repaint.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    } else if let Some(outcome) = &export.outcome {
        match outcome {
            Outcome::Saved(path) => {
                ui.small(format!(
                    "{}: {}",
                    language.text(Text::ExportSaved),
                    path.display()
                ));
            }
            Outcome::Cancelled => {
                ui.small(language.text(Text::ExportCancelled));
            }
            Outcome::Failed(error) => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("{}: {error}", language.text(Text::ExportFailed)),
                );
            }
        }
    }
}

fn snapshot(s11: &S11State, version: Version) -> Result<Document, String> {
    let trace = s11.trace.as_ref().ok_or("no completed S11 sweep")?;
    Document::s1p(trace, version).map_err(|error| error.to_string())
}

fn destination(mut path: PathBuf, language: Language) -> Result<PathBuf, String> {
    if path.extension().is_none() {
        path.set_extension("s1p");
    }
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("s1p"))
    {
        return Err(language.text(Text::WrongExportExtension).into());
    }
    Ok(path)
}

fn choose_and_save(document: Document, language: Language) -> Result<Outcome, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title(language.text(Text::ExportS1p))
        .add_filter("Touchstone S11", &["s1p"])
        .set_file_name("s11.s1p")
        .save_file()
    else {
        return Ok(Outcome::Cancelled);
    };
    // Some native dialogs do not append/enforce the selected extension.
    let path = destination(path, language)?;
    let overwrite = path.try_exists().map_err(|error| error.to_string())?;
    if overwrite
        && rfd::MessageDialog::new()
            .set_title(language.text(Text::ReplaceFile))
            .set_description(path.display().to_string())
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show()
            != rfd::MessageDialogResult::Yes
    {
        return Ok(Outcome::Cancelled);
    }
    document
        .save(&path, overwrite)
        .map_err(|error| error.to_string())?;
    Ok(Outcome::Saved(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::protocol::StreamMode;

    #[test]
    fn snapshot_uses_actual_trace_not_display_controls_and_is_frozen() {
        let mut s11 = S11State {
            trace: Some(SweepData {
                mode: StreamMode::S11,
                format: "ri".into(),
                points: vec![
                    SweepPoint {
                        freq_hz: 5000.0,
                        values: vec![0.5, -0.25],
                    },
                    SweepPoint {
                        freq_hz: 7000000200.0,
                        values: vec![0.1, 0.2],
                    },
                ],
            }),
            ..S11State::default()
        };
        let first = snapshot(&s11, Version::V2).unwrap();
        s11.log_x = true;
        s11.impedance_visible = [false; 3];
        s11.start_hz = 1e6;
        s11.stop_hz = 2e6;
        s11.display = crate::state::S11Display::Vswr;
        assert_eq!(
            first.as_str(),
            snapshot(&s11, Version::V2).unwrap().as_str()
        );
        s11.trace.as_mut().unwrap().points.clear();
        assert!(snapshot(&s11, Version::V2).is_err());
        assert!(first.as_str().contains("[Number of Frequencies] 2"));
        assert!(first.as_str().contains("7.0000002000000000e9"));
        assert!(snapshot(&S11State::default(), Version::V2).is_err());
    }

    #[test]
    fn destination_does_not_mislabel_one_port_or_change_an_existing_extension() {
        for language in Language::ALL {
            assert_eq!(
                destination("s11".into(), language).unwrap(),
                PathBuf::from("s11.s1p")
            );
            assert_eq!(
                destination("s11.S1P".into(), language).unwrap(),
                PathBuf::from("s11.S1P")
            );
            assert!(destination("s11.s2p".into(), language).is_err());
            assert!(destination("s11.csv".into(), language).is_err());
        }
    }

    #[test]
    fn export_events_do_not_change_measurement_state() {
        let mut state = crate::state::AppState::default();
        state.s11.running = true;
        for outcome in [
            Outcome::Cancelled,
            Outcome::Failed("disk full".into()),
            Outcome::Saved("s11.s1p".into()),
        ] {
            let (sender, receiver) = mpsc::channel();
            state.export.pending = Some(receiver);
            state.export.poll();
            assert!(state.export.pending.is_some());
            sender.send(outcome).unwrap();
            state.export.poll();
            assert!(state.export.pending.is_none());
            assert!(state.export.outcome.is_some());
            assert!(state.s11.running);
            assert!(state.status_message.is_none());
        }
    }

    #[test]
    fn disconnected_panel_renders_export_and_translated_guidance_in_bounds() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 600.0));
            let mut state = crate::state::AppState {
                language,
                ..Default::default()
            };
            // Panels settle their content-sized height over successive frames.
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ui| {
                        egui::Panel::right("params_panel")
                            .default_size(260.0)
                            .show(ui, |ui| {
                                crate::panels::s11_panel::show(ui, &mut state);
                            });
                    },
                );
                for text in [Text::ExportS1p, Text::ExportNeedsComplex] {
                    let label = output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::Text(label)
                                if label.galley.job.text == language.text(text) =>
                            {
                                Some(label)
                            }
                            _ => None,
                        })
                        .expect("export controls must remain visible after disconnecting");
                    let bounds = label.galley.rect.translate(label.pos.to_vec2());
                    assert!(screen.contains_rect(bounds), "{language:?}: {bounds:?}");
                }
                output.drop_without_applying_deltas();
            }
        }
    }
}
