// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Explicit calibration consent and packet-driven legacy KC901V steps.

use kcsdi_core::calibration::{
    CalibrationKind, CalibrationParams, CalibrationPhase, CalibrationPrompt, CalibrationReport,
    UserCalibrationParams,
};
use kcsdi_core::validation::frequency_hz;

use crate::acquisition::TraceId;
use crate::i18n::{Language, Text};
use crate::state::{AppMode, AppState, ConnectionState, DEVICE_MODEL, WorkerCommand};

const KINDS: [CalibrationKind; 4] = [
    CalibrationKind::S11System,
    CalibrationKind::S21System,
    CalibrationKind::S11User,
    CalibrationKind::S21User,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenCalibration {
    pub params: CalibrationParams,
    pub trace_id: Option<TraceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    Start,
    Advance,
    Cancel,
}

#[derive(Default)]
pub struct CalibrationUi {
    pub open: bool,
    pub selected: Option<CalibrationKind>,
    pub frozen: Option<FrozenCalibration>,
    pub consent: bool,
    pub report: CalibrationReport,
    pub pending: Option<Pending>,
    pub error: Option<String>,
    pub completed_standards: usize,
    pub current_prompt: Option<CalibrationPrompt>,
}

impl CalibrationUi {
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.report.is_active()
    }

    pub fn open(&mut self) {
        self.open = true;
        if self.report.phase == CalibrationPhase::NotStarted && !self.busy() {
            self.consent = false;
        }
    }

    pub fn begin(&mut self, params: CalibrationParams) {
        if self
            .frozen
            .as_ref()
            .is_none_or(|frozen| frozen.params != params)
        {
            self.frozen = Some(FrozenCalibration {
                params,
                trace_id: None,
            });
        }
        self.open = true;
        self.pending = Some(Pending::Start);
        self.error = None;
        self.completed_standards = 0;
        self.current_prompt = None;
        self.report = CalibrationReport {
            kind: Some(params.kind()),
            phase: CalibrationPhase::NotStarted,
        };
    }

    pub fn accept(&mut self, mut report: CalibrationReport) {
        if report.kind.is_none() && report.phase == CalibrationPhase::Unknown {
            report.kind = self.report.kind;
        }
        if self
            .frozen
            .as_ref()
            .is_none_or(|frozen| Some(frozen.params.kind()) != report.kind)
        {
            return;
        }
        let prompts = standard_prompts(self.frozen.as_ref().unwrap().params.kind());
        if let CalibrationPhase::Prompt(prompt) = report.phase {
            if let Some(index) = prompts.iter().position(|candidate| *candidate == prompt) {
                self.completed_standards = index;
                self.current_prompt = Some(prompt);
            }
        } else if report.phase == CalibrationPhase::Completed {
            self.completed_standards = prompts.len();
            self.current_prompt = None;
        }
        self.report = report;
        self.pending = None;
    }

    pub fn lost(&mut self) {
        if self.busy() {
            self.report.phase = CalibrationPhase::Unknown;
            self.open = true;
        }
        self.pending = None;
    }

    pub fn label(&self, language: Language) -> &'static str {
        language.text(match self.pending {
            Some(Pending::Start) => Text::CalibrationStarting,
            Some(Pending::Advance) => Text::CalibrationMeasuring,
            Some(Pending::Cancel) => Text::CalibrationCancelling,
            None => phase_text(self.report.phase),
        })
    }
}

pub fn affected_mode(kind: CalibrationKind) -> AppMode {
    match kind {
        CalibrationKind::S11System | CalibrationKind::S11User => AppMode::S11,
        CalibrationKind::S21System | CalibrationKind::S21User => AppMode::S21,
    }
}

fn kind_text(kind: CalibrationKind) -> Text {
    match kind {
        CalibrationKind::S11System => Text::CalibrationS11System,
        CalibrationKind::S21System => Text::CalibrationS21System,
        CalibrationKind::S11User => Text::CalibrationS11User,
        CalibrationKind::S21User => Text::CalibrationS21User,
    }
}

fn phase_text(phase: CalibrationPhase) -> Text {
    match phase {
        CalibrationPhase::NotStarted => Text::SourceNotStarted,
        CalibrationPhase::AwaitingConfirmation => Text::CalibrationStarting,
        CalibrationPhase::WarmingUp => Text::CalibrationWarming,
        CalibrationPhase::Prompt(CalibrationPrompt::Short) => Text::CalibrationShort,
        CalibrationPhase::Prompt(CalibrationPrompt::Open) => Text::CalibrationOpen,
        CalibrationPhase::Prompt(CalibrationPrompt::Load) => Text::CalibrationLoad,
        CalibrationPhase::Prompt(CalibrationPrompt::Through) => Text::CalibrationThrough,
        CalibrationPhase::Measuring => Text::CalibrationMeasuring,
        CalibrationPhase::Processing => Text::CalibrationProcessing,
        CalibrationPhase::Saving => Text::CalibrationSaving,
        CalibrationPhase::Completed => Text::CalibrationCompleted,
        CalibrationPhase::Cancelled => Text::CalibrationCancelled,
        CalibrationPhase::Unknown => Text::CalibrationUnknown,
    }
}

fn standard_prompts(kind: CalibrationKind) -> &'static [CalibrationPrompt] {
    match kind {
        CalibrationKind::S11System | CalibrationKind::S11User => &[
            CalibrationPrompt::Short,
            CalibrationPrompt::Open,
            CalibrationPrompt::Load,
        ],
        CalibrationKind::S21System | CalibrationKind::S21User => &[CalibrationPrompt::Through],
    }
}

fn step_rows(ui: &mut egui::Ui, calibration: &CalibrationUi, language: Language) {
    let Some(frozen) = &calibration.frozen else {
        return;
    };
    for (index, &prompt) in standard_prompts(frozen.params.kind()).iter().enumerate() {
        let (status, color) = if index < calibration.completed_standards {
            (Text::CalibrationStepDone, crate::theme::SUCCESS)
        } else if calibration.current_prompt == Some(prompt) {
            if matches!(
                calibration.report.phase,
                CalibrationPhase::Unknown | CalibrationPhase::Cancelled
            ) {
                (Text::CalibrationStepInterrupted, crate::theme::WARN)
            } else {
                (Text::CalibrationStepCurrent, ui.visuals().text_color())
            }
        } else {
            (Text::CalibrationStepPending, ui.visuals().weak_text_color())
        };
        ui.horizontal(|ui| {
            ui.colored_label(
                color,
                format!(
                    "{}. {}",
                    index + 1,
                    language.text(phase_text(CalibrationPhase::Prompt(prompt)))
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(language.text(status));
            });
        });
    }
}

fn freeze(state: &AppState, kind: CalibrationKind) -> Result<FrozenCalibration, String> {
    let error = |key| state.language.text(key).to_owned();
    let mut trace_id = None;
    let params = if kind.is_user() {
        if state.function.kind().is_some() {
            return Err(error(Text::CalibrationMeasurementsOnly));
        }
        if state.workspace.list_mode {
            return Err(error(Text::CalibrationContinuousOnly));
        }
        let trace = state
            .workspace
            .selected()
            .ok_or_else(|| error(Text::CalibrationSelectTrace))?;
        if trace.settings.display.mode() != affected_mode(kind) {
            return Err(error(Text::CalibrationSelectTrace));
        }
        let range = state.workspace.range;
        let start = frequency_hz(range.start_hz, "start").map_err(|error| error.to_string())?;
        let stop = frequency_hz(range.stop_hz, "stop").map_err(|error| error.to_string())?;
        let user = UserCalibrationParams::from_range(start, stop, range.points, trace.settings.rbw)
            .map_err(|error| error.to_string())?;
        trace_id = Some(trace.id);
        match kind {
            CalibrationKind::S11User => CalibrationParams::S11User(user),
            CalibrationKind::S21User => CalibrationParams::S21User(user),
            _ => unreachable!(),
        }
    } else {
        match kind {
            CalibrationKind::S11System => CalibrationParams::S11System,
            CalibrationKind::S21System => CalibrationParams::S21System,
            _ => unreachable!(),
        }
    };
    params
        .validate(&DEVICE_MODEL.capabilities())
        .map_err(|error| error.to_string())?;
    Ok(FrozenCalibration { params, trace_id })
}

fn summary(ui: &mut egui::Ui, frozen: &FrozenCalibration, language: Language) {
    ui.strong(language.text(kind_text(frozen.params.kind())));
    match frozen.params {
        CalibrationParams::S11System | CalibrationParams::S21System => {
            ui.label(language.text(Text::CalibrationSystemRange));
            ui.label(format!(
                "RBW: {}",
                frozen.params.rbw(&DEVICE_MODEL.capabilities()).as_str()
            ));
        }
        CalibrationParams::S11User(params) | CalibrationParams::S21User(params) => {
            if let Some(id) = frozen.trace_id {
                ui.label(format!("T{}", id.0));
            }
            egui::Grid::new("calibration_range").show(ui, |ui| {
                for (key, value) in [
                    (Text::Center, format!("{} Hz", params.center_hz)),
                    (Text::Span, format!("{} Hz", params.span_hz)),
                    (Text::Points, params.points.to_string()),
                    (Text::Rbw, params.rbw.as_str().to_owned()),
                ] {
                    ui.label(language.text(key));
                    ui.monospace(value);
                    ui.end_row();
                }
            });
        }
    }
}

pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.calibration.open {
        return;
    }
    let mut calibration = std::mem::take(&mut state.calibration);
    let language = state.language;
    let mut command = None;
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("calibration_wizard")).show(ctx, |ui| {
        ui.set_width(500.0);
        ui.heading(language.text(Text::CalibrationWizard));
        if calibration.frozen.is_none() {
            for kind in KINDS {
                if kind.is_user() && state.function.kind().is_some() {
                    continue;
                }
                let candidate = freeze(state, kind);
                let response = ui.add_enabled(
                    candidate.is_ok(),
                    egui::Button::selectable(
                        calibration.selected == Some(kind),
                        language.text(kind_text(kind)),
                    ),
                );
                if let Err(error) = candidate {
                    response.on_disabled_hover_text(error);
                } else if response.clicked() {
                    calibration.selected = Some(kind);
                }
            }
            if state.workspace.list_mode && state.function.kind().is_none() {
                ui.small(language.text(Text::CalibrationContinuousOnly));
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        calibration.selected.is_some(),
                        egui::Button::new(language.text(Text::Next)),
                    )
                    .clicked()
                    && let Some(kind) = calibration.selected
                {
                    match freeze(state, kind) {
                        Ok(frozen) => {
                            calibration.frozen = Some(frozen);
                            calibration.consent = false;
                        }
                        Err(error) => calibration.error = Some(error),
                    }
                }
                close = ui.button(language.text(Text::Cancel)).clicked();
            });
        } else if let Some(frozen) = calibration.frozen.clone() {
            summary(ui, &frozen, language);
            ui.separator();
            if calibration.report.phase == CalibrationPhase::NotStarted
                && calibration.pending.is_none()
            {
                ui.label(language.text(Text::CalibrationWriteWarning));
                ui.label(language.text(Text::CalibrationKitHelp));
                ui.checkbox(
                    &mut calibration.consent,
                    language.text(Text::CalibrationConsent),
                );
                let can_start = calibration.consent
                    && state.connection == ConnectionState::Connected
                    && !state.source.busy();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(language.text(Text::CalibrationStart)),
                        )
                        .clicked()
                    {
                        command = Some(WorkerCommand::StartCalibration(frozen.params));
                    }
                    if ui.button(language.text(Text::Back)).clicked() {
                        calibration.frozen = None;
                        calibration.consent = false;
                    }
                    close = ui.button(language.text(Text::Cancel)).clicked();
                });
                if state.source.busy() {
                    ui.small(language.text(Text::CalibrationStopSource));
                } else if state.connection != ConnectionState::Connected {
                    ui.small(language.text(Text::DeviceInfoUnavailable));
                }
            } else {
                ui.horizontal(|ui| {
                    if calibration.busy()
                        && !matches!(calibration.report.phase, CalibrationPhase::Prompt(_))
                    {
                        ui.spinner();
                    }
                    ui.strong(calibration.label(language));
                });
                step_rows(ui, &calibration, language);
                if matches!(calibration.report.phase, CalibrationPhase::Prompt(_))
                    && calibration.pending.is_none()
                {
                    ui.label(language.text(Text::CalibrationConnectHelp));
                }
                if calibration.report.phase == CalibrationPhase::Completed {
                    ui.label(language.text(Text::CalibrationReacquire));
                } else if matches!(
                    calibration.report.phase,
                    CalibrationPhase::Cancelled | CalibrationPhase::Unknown
                ) {
                    ui.label(language.text(Text::CalibrationChanged));
                }
                ui.horizontal(|ui| {
                    if calibration.busy() {
                        if let CalibrationPhase::Prompt(prompt) = calibration.report.phase
                            && ui
                                .add_enabled(
                                    calibration.pending.is_none(),
                                    egui::Button::new(language.text(Text::Next)),
                                )
                                .clicked()
                        {
                            command = Some(WorkerCommand::AdvanceCalibration(prompt));
                        }
                        if ui
                            .add_enabled(
                                calibration.pending != Some(Pending::Cancel),
                                egui::Button::new(language.text(Text::Cancel)),
                            )
                            .clicked()
                        {
                            command = Some(WorkerCommand::CancelCalibration);
                        }
                    } else {
                        if ui.button(language.text(Text::CalibrationNew)).clicked() {
                            calibration = CalibrationUi {
                                open: true,
                                ..Default::default()
                            };
                        }
                        close = ui.button(language.text(Text::Close)).clicked();
                    }
                });
            }
        }
        if let Some(error) = &calibration.error {
            ui.add(
                egui::Label::new(egui::RichText::new(error).color(ui.visuals().error_fg_color))
                    .truncate(),
            )
            .on_hover_text(error);
        }
    });
    if response.should_close() || close {
        if calibration.busy() {
            if calibration.pending != Some(Pending::Cancel) {
                command = Some(WorkerCommand::CancelCalibration);
            }
        } else {
            calibration.open = false;
            calibration.consent = false;
        }
    }
    state.calibration = calibration;
    if let Some(command) = command {
        state.send(command);
    }
}

#[cfg(test)]
mod tests;
