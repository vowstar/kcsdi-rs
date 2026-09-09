// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Frozen measurement exports, with one owned background task at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use kcsdi_core::control::CancellationToken;
use kcsdi_core::touchstone::{self, Document, Version};

use crate::acquisition::{CompletedSweep, TraceId};
use crate::i18n::{Language, Text};
use crate::spreadsheet::FrozenSnapshots;
use crate::workspace::{TraceState, Workspace};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Format {
    #[default]
    Csv,
    Xlsx,
    Touchstone1,
    Touchstone2,
}

impl Format {
    const ALL: [Self; 4] = [Self::Csv, Self::Xlsx, Self::Touchstone1, Self::Touchstone2];

    fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Xlsx => "XLSX",
            Self::Touchstone1 => "Touchstone 1.0",
            Self::Touchstone2 => "Touchstone 2.0",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Xlsx => "xlsx",
            Self::Touchstone1 | Self::Touchstone2 => "s1p",
        }
    }

    fn version(self) -> Option<Version> {
        match self {
            Self::Touchstone1 => Some(Version::V1),
            Self::Touchstone2 => Some(Version::V2),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Scope {
    #[default]
    Selected,
    Visible,
}

impl Scope {
    fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Selected => Text::ExportSelected,
            Self::Visible => Text::ExportVisible,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Summary {
    selected: Option<TraceId>,
    raw: Option<(&'static str, &'static str)>,
    traces: usize,
    points: usize,
}

impl Summary {
    fn label(self, language: Language) -> String {
        let mut source = self.selected.map_or_else(
            || format!("{} {}", self.traces, language.text(Text::ExportTraces)),
            |id| format!("T{}", id.0),
        );
        if let Some((mode, format)) = self.raw {
            source.push(' ');
            source.push_str(&mode.to_ascii_uppercase());
            if !format.is_empty() {
                source.push(' ');
                source.push_str(format);
            }
        }
        format!(
            "{source}, {} {}",
            self.points,
            language.text(Text::ExportPoints)
        )
    }
}

struct Request {
    format: Format,
    snapshots: FrozenSnapshots,
    reflection: Option<Arc<CompletedSweep>>,
    summary: Summary,
}

impl Request {
    fn bytes(&self) -> Result<Vec<u8>, String> {
        match self.format {
            Format::Csv => self.snapshots.csv_bytes(),
            Format::Xlsx => self.snapshots.xlsx_bytes(),
            Format::Touchstone1 | Format::Touchstone2 => {
                let snapshot = self.reflection.as_ref().ok_or("missing S11 snapshot")?;
                Document::s1p(&snapshot.data, self.format.version().unwrap())
                    .map(|document| document.as_str().as_bytes().to_vec())
                    .map_err(|error| error.to_string())
            }
        }
    }

    fn file_name(&self) -> String {
        let name = self
            .summary
            .selected
            .map_or_else(|| "traces".to_owned(), |id| format!("t{}", id.0));
        format!("{name}.{}", self.format.extension())
    }
}

#[derive(Default)]
pub struct ExportState {
    format: Format,
    scope: Scope,
    worker: Option<JoinHandle<Outcome>>,
    cancellation: CancellationToken,
    summary: Option<Summary>,
    outcome: Option<Outcome>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Saved(PathBuf),
    Cancelled,
    Failed(String),
}

impl ExportState {
    pub fn is_pending(&self) -> bool {
        self.worker.is_some()
    }

    /// Native dialogs must return before cancellation can finish the task.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn poll(&mut self) {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            self.outcome = Some(
                self.worker
                    .take()
                    .unwrap()
                    .join()
                    .unwrap_or_else(|_| Outcome::Failed("export worker panicked".into())),
            );
        }
    }

    fn start(&mut self, request: Request, language: Language, ctx: egui::Context) {
        if self.is_pending() {
            return;
        }
        self.summary = Some(request.summary);
        self.spawn(ctx, move |cancel| {
            choose_and_save(request, language, &cancel).unwrap_or_else(Outcome::Failed)
        });
    }

    fn spawn(
        &mut self,
        ctx: egui::Context,
        run: impl FnOnce(CancellationToken) -> Outcome + Send + 'static,
    ) {
        if self.is_pending() {
            return;
        }
        self.cancellation = CancellationToken::default();
        let cancellation = self.cancellation.clone();
        self.outcome = None;
        match std::thread::Builder::new()
            .name("snapshot-export".into())
            .spawn(move || {
                let outcome = run(cancellation);
                ctx.request_repaint();
                outcome
            }) {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => self.outcome = Some(Outcome::Failed(error.to_string())),
        }
    }
}

impl Drop for ExportState {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn selection(workspace: &Workspace, scope: Scope) -> Vec<&TraceState> {
    match scope {
        Scope::Selected => workspace.selected().into_iter().collect(),
        Scope::Visible => workspace
            .traces
            .iter()
            .filter(|trace| trace.settings.visible)
            .collect(),
    }
}

fn missing_message(traces: &[&TraceState], language: Language) -> Option<String> {
    let missing: Vec<_> = traces
        .iter()
        .filter(|trace| trace.completed.is_none())
        .map(|trace| format!("T{}", trace.id.0))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "{}: {}",
            language.text(Text::ExportMissing),
            missing.join(", ")
        )
    })
}

fn freeze(
    workspace: &Workspace,
    format: Format,
    scope: Scope,
    language: Language,
) -> Result<Request, String> {
    let scope = if format.version().is_some() {
        Scope::Selected
    } else {
        scope
    };
    let traces = selection(workspace, scope);
    if traces.is_empty() {
        return Err(language.text(Text::ExportEmpty).into());
    }
    if let Some(message) = missing_message(&traces, language) {
        return Err(message);
    }
    let reflection = format
        .version()
        .is_some()
        .then(|| traces[0].completed.as_ref().unwrap().clone());
    if let Some(reflection) = &reflection {
        touchstone::validate_s1p(&reflection.data).map_err(|error| error.to_string())?;
    }
    let snapshots = FrozenSnapshots::new(
        traces
            .iter()
            .map(|trace| (trace.id, trace.completed.as_ref().unwrap().clone()))
            .collect(),
    )?;
    let summary = Summary {
        selected: (scope == Scope::Selected).then_some(traces[0].id),
        raw: selected_raw(&traces, scope),
        traces: snapshots.len(),
        points: snapshots.point_count(),
    };
    Ok(Request {
        format,
        snapshots,
        reflection,
        summary,
    })
}

fn selected_raw(traces: &[&TraceState], scope: Scope) -> Option<(&'static str, &'static str)> {
    if scope != Scope::Selected {
        return None;
    }
    let snapshot = traces.first()?.completed.as_ref()?;
    Some((snapshot.settings.mode().name(), snapshot.settings.format()))
}

pub fn show(
    ui: &mut egui::Ui,
    export: &mut ExportState,
    workspace: &Workspace,
    language: Language,
) {
    let busy = export.is_pending();
    ui.add_enabled_ui(!busy, |ui| {
        egui::Grid::new("export_options")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label(language.text(Text::ExportFormat));
                egui::ComboBox::from_id_salt("export_format")
                    .width(160.0)
                    .selected_text(export.format.label())
                    .show_ui(ui, |ui| {
                        for format in Format::ALL {
                            ui.selectable_value(&mut export.format, format, format.label());
                        }
                    });
                ui.end_row();
                ui.label(language.text(Text::ExportScope));
                let effective_scope = if export.format.version().is_some() {
                    Scope::Selected
                } else {
                    export.scope
                };
                ui.add_enabled_ui(export.format.version().is_none(), |ui| {
                    egui::ComboBox::from_id_salt("export_scope")
                        .width(160.0)
                        .selected_text(effective_scope.label(language))
                        .show_ui(ui, |ui| {
                            for scope in [Scope::Selected, Scope::Visible] {
                                ui.selectable_value(
                                    &mut export.scope,
                                    scope,
                                    scope.label(language),
                                );
                            }
                        });
                });
                ui.end_row();
            });
    });
    let scope = if export.format.version().is_some() {
        Scope::Selected
    } else {
        export.scope
    };
    let traces = selection(workspace, scope);
    let missing = missing_message(&traces, language);
    let validation = export.format.version().and_then(|_| {
        traces
            .first()
            .and_then(|trace| trace.completed.as_ref())
            .map(|snapshot| touchstone::validate_s1p(&snapshot.data))
    });
    let complex = export.format.version().is_none() || matches!(validation, Some(Ok(())));
    let ready = !traces.is_empty() && missing.is_none() && complex;
    let button = ui
        .add_enabled_ui(
            if busy {
                !export.cancellation.is_cancelled()
            } else {
                ready
            },
            |ui| {
                ui.add_sized(
                    [ui.available_width(), 24.0],
                    egui::Button::new(language.text(if busy {
                        Text::Cancel
                    } else if export.format.version().is_some() {
                        Text::ExportS1p
                    } else {
                        Text::Export
                    })),
                )
            },
        )
        .inner;
    let button = if let Some(Err(error)) = validation {
        button.on_disabled_hover_text(error.to_string())
    } else {
        button
    };
    if button
        .on_hover_text(language.text(if busy {
            Text::ExportCancelHelp
        } else {
            Text::ExportHelp
        }))
        .clicked()
    {
        if busy {
            export.cancel();
        } else {
            match freeze(workspace, export.format, scope, language) {
                Ok(request) => export.start(request, language, ui.ctx().clone()),
                Err(error) => export.outcome = Some(Outcome::Failed(error)),
            }
        }
    }
    if export.is_pending() {
        if let Some(summary) = export.summary {
            clipped_label(ui, summary.label(language), false);
        }
        clipped_label(
            ui,
            language
                .text(if export.cancellation.is_cancelled() {
                    Text::ExportCancelling
                } else {
                    Text::ExportBusy
                })
                .into(),
            false,
        );
        if export.cancellation.is_cancelled() {
            clipped_label(ui, language.text(Text::ExportCancelHelp).into(), false);
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(50));
    } else {
        let summary = Summary {
            selected: (scope == Scope::Selected)
                .then(|| traces.first().map(|trace| trace.id))
                .flatten(),
            raw: selected_raw(&traces, scope),
            traces: traces.len(),
            points: traces
                .iter()
                .filter_map(|trace| trace.completed.as_ref())
                .map(|snapshot| snapshot.data.points.len())
                .sum(),
        };
        clipped_label(
            ui,
            missing.unwrap_or_else(|| {
                if traces.is_empty() {
                    language.text(Text::ExportEmpty).into()
                } else if !complex {
                    language.text(Text::ExportNeedsComplex).into()
                } else {
                    summary.label(language)
                }
            }),
            false,
        );
        if let Some(outcome) = &export.outcome {
            match outcome {
                Outcome::Saved(path) => clipped_label(
                    ui,
                    format!("{}: {}", language.text(Text::ExportSaved), path.display()),
                    false,
                ),
                Outcome::Cancelled => {
                    clipped_label(ui, language.text(Text::ExportCancelled).into(), false)
                }
                Outcome::Failed(error) => clipped_label(
                    ui,
                    format!("{}: {error}", language.text(Text::ExportFailed)),
                    true,
                ),
            }
        }
    }
}

fn clipped_label(ui: &mut egui::Ui, text: String, error: bool) {
    let mut rich = egui::RichText::new(&text).small();
    if error {
        rich = rich.color(egui::Color32::LIGHT_RED);
    }
    ui.add(egui::Label::new(rich).truncate())
        .on_hover_text(text);
}

fn destination(mut path: PathBuf, format: Format, language: Language) -> Result<PathBuf, String> {
    let extension = format.extension();
    if path.extension().is_none() {
        path.set_extension(extension);
    }
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
    {
        return Err(format!(
            "{}: .{extension}",
            language.text(Text::WrongExportExtension)
        ));
    }
    Ok(path)
}

fn choose_and_save(
    request: Request,
    language: Language,
    cancel: &CancellationToken,
) -> Result<Outcome, String> {
    let format = request.format;
    let file_name = request.file_name();
    save_with_dialogs(
        request,
        language,
        cancel,
        || {
            rfd::FileDialog::new()
                .set_title(language.text(Text::Export))
                .add_filter(format.label(), &[format.extension()])
                .set_file_name(file_name)
                .save_file()
        },
        |path| {
            rfd::MessageDialog::new()
                .set_title(language.text(Text::ReplaceFile))
                .set_description(path.display().to_string())
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::YesNo)
                .show()
                == rfd::MessageDialogResult::Yes
        },
    )
}

fn save_with_dialogs(
    request: Request,
    language: Language,
    cancel: &CancellationToken,
    choose: impl FnOnce() -> Option<PathBuf>,
    confirm: impl FnOnce(&Path) -> bool,
) -> Result<Outcome, String> {
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let bytes = request.bytes()?;
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let selected = choose();
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let Some(path) = selected else {
        return Ok(Outcome::Cancelled);
    };
    let path = destination(path, request.format, language)?;
    let overwrite = path.try_exists().map_err(|error| error.to_string())?;
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    if overwrite {
        let approved = confirm(&path);
        if cancel.is_cancelled() || !approved {
            return Ok(Outcome::Cancelled);
        }
    }
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    kcsdi_core::atomic_file::write(&path, &bytes, overwrite).map_err(|error| error.to_string())?;
    Ok(Outcome::Saved(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::AcquisitionSettings;
    use crate::workspace::{TraceDisplay, TraceSettings};
    use kcsdi_core::commands::Format as WireFormat;
    use kcsdi_core::data::{SweepData, SweepPoint};
    use kcsdi_core::protocol::StreamMode;
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime};

    fn completed() -> Arc<CompletedSweep> {
        Arc::new(CompletedSweep {
            data: SweepData {
                mode: StreamMode::S11,
                format: "ri".into(),
                points: [1e6, 2e6, 3_000_000.2]
                    .into_iter()
                    .map(|freq_hz| SweepPoint {
                        freq_hz,
                        values: vec![0.5, -0.25],
                    })
                    .collect(),
            },
            settings: AcquisitionSettings::S11(kcsdi_core::device::S11Params {
                format: WireFormat::Ri,
                start_hz: 1_000_000,
                stop_hz: 3_000_000,
                points: 3,
                ..crate::acquisition::tests::s11()
            }),
            session_id: 1,
            completed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(123),
        })
    }

    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.selected_mut().unwrap().completed = Some(completed());
        workspace
    }

    fn request(format: Format) -> Request {
        freeze(&workspace(), format, Scope::Selected, Language::English).unwrap()
    }

    fn finish(export: &mut ExportState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while export.is_pending() && Instant::now() < deadline {
            export.poll();
            std::thread::yield_now();
        }
        assert!(!export.is_pending(), "export did not finish");
    }

    #[test]
    fn every_export_freezes_actual_complete_data_and_its_identity() {
        for format in Format::ALL {
            let mut workspace = workspace();
            let id = workspace.selected.unwrap();
            let frozen = freeze(&workspace, format, Scope::Selected, Language::English).unwrap();
            let before = frozen.snapshots.csv_bytes().unwrap();
            let old = workspace.selected().unwrap().completed.clone().unwrap();
            let mut prefix = old.data.clone();
            prefix.points.truncate(1);
            prefix.points[0].values[0] = 0.9;
            workspace.selected_mut().unwrap().preview =
                Some(Arc::new(crate::preview::PreviewEnvelope {
                    session_id: 1,
                    request_id: 1,
                    cycle_id: 2,
                    data: prefix,
                    group: crate::acquisition::AcquisitionGroup {
                        settings: old.settings.clone(),
                        members: vec![id],
                    },
                }));
            assert_eq!(
                before,
                freeze(&workspace, format, Scope::Selected, Language::English)
                    .unwrap()
                    .snapshots
                    .csv_bytes()
                    .unwrap()
            );
            workspace.log_x = true;
            workspace.range.start_hz = 10e6;
            workspace.range.stop_hz = 20e6;
            let trace = workspace.selected_mut().unwrap();
            trace.settings.visible = false;
            trace.settings.impedance_visible = [false; 3];
            trace.settings.display = TraceDisplay::S11(crate::state::S11Display::Vswr);
            assert_eq!(
                selected_raw(&selection(&workspace, Scope::Selected), Scope::Selected),
                Some(("s11", "ri"))
            );
            for language in Language::ALL {
                assert_eq!(
                    frozen.summary.label(language),
                    format!("T{} S11 ri, 3 {}", id.0, language.text(Text::ExportPoints))
                );
            }
            assert_eq!(
                before,
                freeze(&workspace, format, Scope::Selected, Language::English)
                    .unwrap()
                    .snapshots
                    .csv_bytes()
                    .unwrap()
            );
            let mut newer = (*old).clone();
            newer.data.points[0].values[0] = 0.75;
            workspace.selected_mut().unwrap().completed = Some(Arc::new(newer));
            workspace.remove_trace(id);
            assert!(workspace.traces.is_empty());
            assert_eq!(frozen.summary.selected, Some(id));
            assert_eq!(frozen.summary.points, 3);
            assert_eq!(before, frozen.snapshots.csv_bytes().unwrap());
            let bytes = frozen.bytes().unwrap();
            assert!(!bytes.is_empty());
            if format.version().is_some() {
                assert!(
                    String::from_utf8(bytes)
                        .unwrap()
                        .contains("3.0000002000000002e6")
                );
            }
        }
    }

    #[test]
    fn visible_scope_counts_trace_definitions_not_hidden_components() {
        let mut workspace = workspace();
        let first = workspace.selected.unwrap();
        workspace.selected_mut().unwrap().settings.impedance_visible = [false; 3];
        let second = workspace.add_trace(TraceSettings::default()).unwrap();
        workspace.selected_mut().unwrap().completed = Some(completed());
        let visible = freeze(&workspace, Format::Csv, Scope::Visible, Language::English).unwrap();
        assert_eq!((visible.summary.traces, visible.summary.points), (2, 6));
        assert_eq!(visible.summary.selected, None);
        workspace.traces[0].settings.visible = false;
        let visible = freeze(&workspace, Format::Xlsx, Scope::Visible, Language::English).unwrap();
        assert_eq!(visible.summary.traces, 1);
        workspace.selected = Some(first);
        let selected = freeze(&workspace, Format::Csv, Scope::Selected, Language::English).unwrap();
        assert_eq!(selected.summary.selected, Some(first));
        let reflection = freeze(
            &workspace,
            Format::Touchstone2,
            Scope::Visible,
            Language::English,
        )
        .unwrap();
        assert_eq!(reflection.summary.selected, Some(first));
        assert_ne!(reflection.summary.selected, Some(second));
    }

    #[test]
    fn selection_rejects_empty_missing_and_malformed_complete_frames_before_dialogs() {
        for language in Language::ALL {
            let mut workspace = workspace();
            let first = workspace.selected.unwrap();
            let missing = workspace.add_trace(TraceSettings::default()).unwrap();
            let error = freeze(&workspace, Format::Csv, Scope::Visible, language)
                .err()
                .unwrap();
            assert!(error.contains(language.text(Text::ExportMissing)));
            assert!(error.contains(&format!("T{}", missing.0)));
            workspace.selected = Some(first);
            assert!(freeze(&workspace, Format::Csv, Scope::Selected, language).is_ok());
            workspace.traces[1].settings.visible = false;
            assert!(freeze(&workspace, Format::Csv, Scope::Visible, language).is_ok());
            let mut bad = (*completed()).clone();
            bad.data.points[0].values.clear();
            workspace.traces[0].completed = Some(Arc::new(bad));
            assert!(freeze(&workspace, Format::Csv, Scope::Selected, language).is_err());
            workspace.traces[0].settings.visible = false;
            assert_eq!(
                freeze(&workspace, Format::Csv, Scope::Visible, language)
                    .err()
                    .unwrap(),
                language.text(Text::ExportEmpty)
            );
            workspace.remove_trace(first);
            workspace.remove_trace(missing);
            assert!(freeze(&workspace, Format::Xlsx, Scope::Selected, language).is_err());
        }
    }

    #[test]
    fn transmission_data_is_not_a_one_port_reflection_export() {
        let mut workspace = workspace();
        let mut data = (*completed()).clone();
        data.data.mode = StreamMode::S21;
        data.settings = AcquisitionSettings::S21(kcsdi_core::device::S21Params {
            format: WireFormat::Ri,
            start_hz: 1_000_000,
            stop_hz: 3_000_000,
            points: 3,
            ..crate::acquisition::tests::s21()
        });
        workspace.selected_mut().unwrap().completed = Some(Arc::new(data));
        assert!(freeze(&workspace, Format::Csv, Scope::Selected, Language::English).is_ok());
        for format in [Format::Touchstone1, Format::Touchstone2] {
            assert!(freeze(&workspace, format, Scope::Selected, Language::English).is_err());
        }
    }

    #[test]
    fn extensions_are_appended_only_when_absent() {
        for language in Language::ALL {
            for format in Format::ALL {
                let extension = format.extension();
                assert_eq!(
                    destination("trace".into(), format, language).unwrap(),
                    PathBuf::from(format!("trace.{extension}"))
                );
                let upper = PathBuf::from(format!("trace.{}", extension.to_ascii_uppercase()));
                assert_eq!(destination(upper.clone(), format, language).unwrap(), upper);
                assert!(destination("trace.s2p".into(), format, language).is_err());
                assert!(destination("trace.txt".into(), format, language).is_err());
            }
        }
    }

    #[test]
    fn native_cancellation_and_overwrite_refusal_never_write() {
        for phase in 0..4 {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("trace.csv");
            std::fs::write(&path, b"keep existing").unwrap();
            let cancel = CancellationToken::default();
            let chose = Cell::new(false);
            let confirmed = Cell::new(false);
            if phase == 0 {
                cancel.cancel();
            }
            let outcome = save_with_dialogs(
                request(Format::Csv),
                Language::English,
                &cancel,
                || {
                    chose.set(true);
                    if phase == 1 {
                        cancel.cancel();
                    }
                    Some(path.clone())
                },
                |actual| {
                    confirmed.set(true);
                    assert_eq!(actual, path);
                    if phase == 2 {
                        cancel.cancel();
                    }
                    phase != 3
                },
            )
            .unwrap();
            assert_eq!(outcome, Outcome::Cancelled);
            assert_eq!(chose.get(), phase != 0);
            assert_eq!(confirmed.get(), phase >= 2);
            assert_eq!(std::fs::read(&path).unwrap(), b"keep existing");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
        let cancel = CancellationToken::default();
        assert_eq!(
            save_with_dialogs(
                request(Format::Csv),
                Language::English,
                &cancel,
                || None,
                |_| panic!("cancelled picker must not confirm")
            )
            .unwrap(),
            Outcome::Cancelled
        );
    }

    #[test]
    fn normalized_destination_is_confirmed_and_write_errors_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let stem = directory.path().join("测量");
        let path = stem.with_extension("csv");
        std::fs::write(&path, b"old").unwrap();
        let cancel = CancellationToken::default();
        let outcome = save_with_dialogs(
            request(Format::Csv),
            Language::SimplifiedChinese,
            &cancel,
            || Some(stem),
            |actual| {
                assert_eq!(actual, path);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Saved(path.clone()));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("trace_id,")
        );
        for path in [
            directory.path().join("missing").join("trace.csv"),
            directory.path().join("folder.csv"),
        ] {
            if path.file_name().unwrap() == "folder.csv" {
                std::fs::create_dir(&path).unwrap();
            }
            assert!(
                save_with_dialogs(
                    request(Format::Csv),
                    Language::English,
                    &cancel,
                    || Some(path.clone()),
                    |_| true
                )
                .is_err()
            );
        }
        assert!(
            save_with_dialogs(
                request(Format::Csv),
                Language::English,
                &cancel,
                || Some(directory.path().join("wrong.xlsx")),
                |_| panic!("wrong extension must not confirm")
            )
            .is_err()
        );
    }

    #[test]
    fn owned_worker_remains_pending_until_finished_and_can_restart_after_cancel() {
        let mut state = crate::state::AppState {
            sweep: crate::state::SweepState::Running,
            ..Default::default()
        };
        let (release, wait) = mpsc::channel();
        state.export.spawn(egui::Context::default(), move |cancel| {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(cancel.is_cancelled());
            Outcome::Cancelled
        });
        state.export.poll();
        assert!(state.export.is_pending());
        state.export.spawn(egui::Context::default(), |_| {
            panic!("second export must not start")
        });
        state.export.cancel();
        state.export.poll();
        assert!(state.export.is_pending());
        release.send(()).unwrap();
        finish(&mut state.export);
        assert_eq!(state.export.outcome, Some(Outcome::Cancelled));
        assert!(state.any_running());
        assert!(state.status_message.is_none());
        state.export.spawn(egui::Context::default(), |cancel| {
            assert!(!cancel.is_cancelled());
            Outcome::Saved("trace.csv".into())
        });
        finish(&mut state.export);
        assert_eq!(
            state.export.outcome,
            Some(Outcome::Saved("trace.csv".into()))
        );
        state
            .export
            .spawn(egui::Context::default(), |_| panic!("test worker panic"));
        finish(&mut state.export);
        assert_eq!(
            state.export.outcome,
            Some(Outcome::Failed("export worker panicked".into()))
        );
    }

    #[test]
    fn bilingual_export_controls_and_long_results_stay_inside_the_fixed_panel() {
        for language in Language::ALL {
            for width in [960.0, 1280.0] {
                for case in 0..8 {
                    let ctx = egui::Context::default();
                    crate::theme::setup(&ctx);
                    let mut state = crate::state::AppState {
                        language,
                        workspace: workspace(),
                        ..Default::default()
                    };
                    state.workspace.selected_mut().unwrap().settings.display =
                        TraceDisplay::S11(crate::state::S11Display::Impedance);
                    let mut release = None;
                    match case {
                        0 => state.workspace.selected_mut().unwrap().completed = None,
                        1 => {
                            state.export.format = Format::Xlsx;
                        }
                        2 => {
                            state.workspace.add_trace(TraceSettings::default()).unwrap();
                            state.export.scope = Scope::Visible;
                        }
                        3 => {
                            state.workspace.selected_mut().unwrap().settings.visible = false;
                            state.export.scope = Scope::Visible;
                        }
                        4 => {
                            state.export.outcome = Some(Outcome::Saved(PathBuf::from(format!(
                                "/exports/{}.csv",
                                "很长的测量数据文件名".repeat(50)
                            ))))
                        }
                        5 => {
                            state.export.outcome = Some(Outcome::Failed(
                                "filesystem failure for a very long path ".repeat(80),
                            ))
                        }
                        6 | 7 => {
                            let (sender, receiver) = mpsc::channel();
                            release = Some(sender);
                            state.export.summary = Some(Summary {
                                selected: Some(TraceId(9)),
                                raw: Some(("s11", "z")),
                                traces: 1,
                                points: 1001,
                            });
                            state.export.spawn(ctx.clone(), move |_| {
                                receiver.recv_timeout(Duration::from_secs(10)).unwrap();
                                Outcome::Cancelled
                            });
                            if case == 7 {
                                state.export.cancel();
                            }
                        }
                        _ => unreachable!(),
                    }
                    let screen =
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0));
                    for _ in 0..3 {
                        let output = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |ui| {
                                crate::app::parameter_panel(ui, &mut state);
                            },
                        );
                        let labels: Vec<_> = output
                            .shapes
                            .iter()
                            .filter_map(|shape| match &shape.shape {
                                egui::Shape::Text(text) => Some((
                                    text.galley.text().to_owned(),
                                    text.galley.rect.translate(text.pos.to_vec2()),
                                )),
                                _ => None,
                            })
                            .collect();
                        let mut required = vec![
                            Text::ExportFormat,
                            Text::ExportScope,
                            if case >= 6 {
                                Text::Cancel
                            } else {
                                Text::Export
                            },
                        ];
                        if case == 7 {
                            required.extend([Text::ExportCancelling, Text::ExportCancelHelp]);
                        }
                        for key in required {
                            let (_, bounds) = labels
                                .iter()
                                .find(|(text, _)| text == language.text(key))
                                .unwrap();
                            assert!(
                                screen.contains_rect(*bounds),
                                "{language:?} case{case}: {key:?} {bounds:?}"
                            );
                            assert!(
                                bounds.left() >= width - 256.0,
                                "{language:?} case{case}: {key:?} starts outside parameter panel {bounds:?}"
                            );
                        }
                        for (text, bounds) in labels.iter().filter(|(text, _)| {
                            text.starts_with(language.text(Text::ExportSaved))
                                || text.starts_with(language.text(Text::ExportFailed))
                                || text.starts_with(language.text(Text::ExportMissing))
                        }) {
                            assert!(
                                screen.contains_rect(*bounds),
                                "clipped export status overflow: {text} {bounds:?}"
                            );
                            assert!(bounds.left() >= width - 256.0);
                        }
                        if case == 7 {
                            let bounds = |key| {
                                labels
                                    .iter()
                                    .find(|(text, _)| text == language.text(key))
                                    .unwrap()
                                    .1
                            };
                            let status = bounds(Text::ExportCancelling);
                            let help = bounds(Text::ExportCancelHelp);
                            assert!(help.top() >= status.bottom());
                            assert!(status.top() >= screen.bottom() - 148.0);
                            for shape in &output.shapes {
                                if let egui::Shape::Text(text) = &shape.shape
                                    && [Text::ExportCancelling, Text::ExportCancelHelp]
                                        .iter()
                                        .any(|key| text.galley.text() == language.text(*key))
                                {
                                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                    assert!(
                                        shape.clip_rect.contains_rect(bounds),
                                        "cancellation guidance was clipped: {language:?} {bounds:?}"
                                    );
                                }
                            }
                        }
                        output.drop_without_applying_deltas();
                    }
                    if let Some(release) = release {
                        release.send(()).unwrap();
                        finish(&mut state.export);
                    }
                }
            }
        }
    }

    #[test]
    fn touchstone_keeps_translated_complex_guidance_and_selected_scope() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let mut workspace = workspace();
            let mut scalar = (*completed()).clone();
            scalar.data.format = "vswr".into();
            for point in &mut scalar.data.points {
                point.values = vec![2.0];
            }
            workspace.selected_mut().unwrap().completed = Some(Arc::new(scalar));
            let mut export = ExportState::default();
            export.format = Format::Touchstone2;
            export.scope = Scope::Visible;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(256.0, 180.0),
                    )),
                    ..Default::default()
                },
                |ui| show(ui, &mut export, &workspace, language),
            );
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect();
            for key in [
                Text::ExportS1p,
                Text::ExportSelected,
                Text::ExportNeedsComplex,
            ] {
                assert!(
                    labels.iter().any(|text| *text == language.text(key)),
                    "missing {language:?} {key:?}"
                );
            }
            assert!(!export.is_pending());
            output.drop_without_applying_deltas();
        }
    }
}
