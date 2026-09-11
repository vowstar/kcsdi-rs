// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Transactional segment drafts. Editing never sends instrument commands.

use kcsdi_core::device::PointSettings;
use kcsdi_core::model::FreqRange;
use kcsdi_core::segments::{MAX_SEGMENTS, Segment, SegmentError, SegmentPlan, SegmentProblem};

use crate::acquisition::TraceId;
use crate::i18n::{Language, Text};
use crate::state::DEVICE_MODEL;
use crate::widgets::plot::{format_axis_value, parse_axis_value};
use crate::workspace::SweepRange;

#[derive(Clone)]
struct Draft {
    fields: [String; 3],
}

impl From<Segment> for Draft {
    fn from(row: Segment) -> Self {
        Self {
            fields: [row.start_hz, row.stop_hz, row.max_step_hz]
                .map(|value| format_axis_value(value as f64, "Hz")),
        }
    }
}

#[derive(Debug)]
pub(crate) struct DraftError {
    pub row: Option<usize>,
    field: Option<usize>,
    adjacent: bool,
    pub text: String,
}

pub(crate) fn error_text(error: &SegmentError, language: Language) -> String {
    let text = match &error.problem {
        SegmentProblem::Count => format!(
            "{}: 1 {} {MAX_SEGMENTS}",
            language.text(Text::SegmentCount),
            language.text(Text::RangeTo)
        ),
        SegmentProblem::Order => language.text(Text::SegmentOrderError).into(),
        SegmentProblem::Step => language.text(Text::SegmentStepError).into(),
        SegmentProblem::Adjacency => language.text(Text::SegmentAdjacentError).into(),
        SegmentProblem::Points { required, maximum } => format!(
            "{}: {required}. {}: {maximum}. {}",
            language.text(Text::SegmentRequiredPoints),
            language.text(Text::SegmentMaximumPoints),
            language.text(Text::SegmentPointsHelp)
        ),
        SegmentProblem::Budget => language.text(Text::SegmentBudgetError).into(),
        SegmentProblem::Receiver(_) => language.text(Text::SegmentReceiverError).into(),
    };
    if let Some(row) = error.row {
        format!("{} {}: {text}", language.text(Text::Segment), row + 1)
    } else {
        text
    }
}

fn whole_hz(text: &str) -> Option<u64> {
    let value = parse_axis_value(text, "Hz")?;
    (value.is_finite() && value >= 0.0 && value < u64::MAX as f64 && value.fract() == 0.0)
        .then_some(value as u64)
}

impl Draft {
    fn parse(&self, row: usize, language: Language) -> Result<Segment, DraftError> {
        let mut values = [0; 3];
        for (field, text) in self.fields.iter().enumerate() {
            values[field] = whole_hz(text).ok_or_else(|| DraftError {
                row: Some(row),
                field: Some(field),
                adjacent: false,
                text: format!(
                    "{} {}: {}",
                    language.text(Text::Segment),
                    row + 1,
                    language.text(Text::SegmentWholeHz)
                ),
            })?;
        }
        Ok(Segment {
            start_hz: values[0],
            stop_hz: values[1],
            max_step_hz: values[2],
        })
    }
}

pub(crate) fn validate(
    rows: &[Segment],
    receivers: &[(TraceId, PointSettings)],
    allowed: FreqRange,
    language: Language,
) -> Result<u32, DraftError> {
    for (index, row) in rows.iter().enumerate() {
        if row.start_hz < row.stop_hz && !allowed.contains_sweep(row.start_hz, row.stop_hz) {
            return Err(DraftError {
                row: Some(index),
                field: None,
                adjacent: false,
                text: format!(
                    "{} {}: {} {} {} {}. {} {}",
                    language.text(Text::Segment),
                    index + 1,
                    language.text(Text::SegmentFrequencyLimits),
                    format_axis_value(allowed.min_hz as f64, "Hz"),
                    language.text(Text::RangeTo),
                    format_axis_value(allowed.max_hz as f64, "Hz"),
                    language.text(Text::SegmentMinimumSpan),
                    format_axis_value(allowed.min_span_hz as f64, "Hz")
                ),
            });
        }
    }
    let mut acquired = None;
    for (id, settings) in receivers {
        let plan =
            SegmentPlan::new(rows, settings, &DEVICE_MODEL.capabilities()).map_err(|error| {
                DraftError {
                    row: error.row,
                    field: match error.problem {
                        SegmentProblem::Order => Some(1),
                        SegmentProblem::Step | SegmentProblem::Points { .. } => Some(2),
                        SegmentProblem::Adjacency => Some(0),
                        _ => None,
                    },
                    adjacent: error.problem == SegmentProblem::Adjacency,
                    text: format!("T{}: {}", id.0, error_text(&error, language)),
                }
            })?;
        acquired = Some(plan.acquired_points());
    }
    acquired.ok_or_else(|| DraftError {
        row: None,
        field: None,
        adjacent: false,
        text: language.text(Text::SegmentSelectTrace).into(),
    })
}

#[derive(Default)]
pub struct SegmentEditor {
    open: bool,
    rows: Vec<Draft>,
}

impl SegmentEditor {
    pub fn open(&mut self, rows: &[Segment], range: &SweepRange) {
        if self.open {
            return;
        }
        self.rows = if rows.is_empty() {
            let step = ((range.stop_hz - range.start_hz)
                / f64::from(range.points.saturating_sub(1).max(1)))
            .ceil();
            vec![Draft {
                fields: [range.start_hz, range.stop_hz, step]
                    .map(|value| format_axis_value(value, "Hz")),
            }]
        } else {
            rows.iter()
                .copied()
                .take(MAX_SEGMENTS)
                .map(Draft::from)
                .collect()
        };
        self.open = true;
    }

    pub fn cancel(&mut self) {
        self.open = false;
    }

    fn definitions(&self, language: Language) -> Result<Vec<Segment>, DraftError> {
        self.rows
            .iter()
            .enumerate()
            .map(|(row, draft)| draft.parse(row, language))
            .collect()
    }

    fn add_next(&mut self) {
        if self.rows.len() >= MAX_SEGMENTS {
            return;
        }
        let (start, step) = self
            .rows
            .last()
            .map(|last| (last.fields[1].clone(), last.fields[2].clone()))
            .unwrap_or_default();
        self.rows.push(Draft {
            fields: [start, String::new(), step],
        });
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        language: Language,
        allowed: FreqRange,
        receivers: &[(TraceId, PointSettings)],
        can_apply: bool,
    ) -> Option<Vec<Segment>> {
        if !self.open {
            return None;
        }
        let mut open = true;
        let mut applied = None;
        let mut cancel = false;
        egui::Window::new(language.text(Text::EditSegments))
            .id(egui::Id::new("segment_editor"))
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_width(820.0)
            .min_width(720.0)
            .default_pos(egui::pos2(
                ((ctx.content_rect().width() - 820.0) / 2.0).max(0.0),
                110.0,
            ))
            .show(ctx, |ui| {
                ui.label(language.text(Text::SegmentsHelp));
                let validation = self
                    .definitions(language)
                    .and_then(|rows| validate(&rows, receivers, allowed, language));
                let error = validation.as_ref().err();
                let mut remove = None;
                let count = self.rows.len();
                egui::ScrollArea::both().max_height(340.0).show(ui, |ui| {
                    egui::Grid::new("segment_table")
                        .num_columns(7)
                        .striped(true)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("");
                            for label in [
                                Text::Start,
                                Text::Stop,
                                Text::SegmentMaxStep,
                                Text::Points,
                                Text::SegmentCalculatedStep,
                            ] {
                                ui.label(language.text(label))
                                    .on_hover_text(language.text(Text::SegmentStepHelp));
                            }
                            ui.label("");
                            ui.end_row();
                            for (index, row) in self.rows.iter_mut().enumerate() {
                                ui.label((index + 1).to_string());
                                for (field, value) in row.fields.iter_mut().enumerate() {
                                    let invalid = error.is_some_and(|error| {
                                        error.row == Some(index)
                                            && error.field.is_none_or(|column| column == field)
                                            || (error.adjacent
                                                && error.row == Some(index + 1)
                                                && field == 1)
                                    });
                                    let mut edit = egui::TextEdit::singleline(value)
                                        .id_salt((index, field))
                                        .desired_width(130.0);
                                    if invalid {
                                        edit = edit.text_color(ui.visuals().error_fg_color);
                                    }
                                    ui.add_sized([130.0, 24.0], edit);
                                }
                                let row = row.parse(index, language).ok();
                                let points = row
                                    .and_then(|row| row.points(&DEVICE_MODEL.capabilities()).ok());
                                ui.label(
                                    points.map_or_else(|| "-".into(), |points| points.to_string()),
                                );
                                ui.label(row.zip(points).map_or_else(
                                    || "-".into(),
                                    |(row, points)| {
                                        format_axis_value(
                                            (row.stop_hz - row.start_hz) as f64
                                                / f64::from(points - 1),
                                            "Hz",
                                        )
                                    },
                                ));
                                if ui
                                    .add_enabled(
                                        count > 1,
                                        egui::Button::new(language.text(Text::Delete)),
                                    )
                                    .clicked()
                                {
                                    remove = Some(index);
                                }
                                ui.end_row();
                            }
                        });
                });
                if let Some(index) = remove {
                    self.rows.remove(index);
                }
                if ui
                    .add_enabled(
                        self.rows.len() < MAX_SEGMENTS,
                        egui::Button::new(language.text(Text::AddSegment)),
                    )
                    .clicked()
                {
                    self.add_next();
                }
                let parsed = self.definitions(language).and_then(|rows| {
                    validate(&rows, receivers, allowed, language).map(|points| (rows, points))
                });
                match &parsed {
                    Ok((rows, points)) => {
                        ui.label(format!(
                            "{}: {}    {}: {points}",
                            language.text(Text::SegmentCount),
                            rows.len(),
                            language.text(Text::SegmentAcquiredPoints)
                        ));
                    }
                    Err(error) => {
                        ui.colored_label(ui.visuals().error_fg_color, &error.text);
                    }
                }
                if !can_apply {
                    ui.small(language.text(Text::SegmentStopToApply));
                }
                ui.small(language.text(Text::SegmentBoundsHelp));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            can_apply && parsed.is_ok(),
                            egui::Button::new(language.text(Text::Apply)),
                        )
                        .clicked()
                    {
                        applied = parsed.ok().map(|(rows, _)| rows);
                    }
                    cancel = ui.button(language.text(Text::Cancel)).clicked();
                });
            });
        self.open = open && !cancel && applied.is_none();
        applied
    }
}

#[cfg(test)]
mod tests;
