// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Session analysis of completed sweeps. Scalar envelopes never become
//! complex acquisition data or replace the snapshot used for export.

use egui::Color32;
use kcsdi_core::data::SweepData;
use std::collections::BTreeMap;

use crate::i18n::{Language, Text};
use crate::widgets::plot::{Marker, Series};

const MAX_MARKERS: usize = 10;

#[derive(Default)]
pub struct AnalysisTools {
    latest: Option<SweepData>,
    hold: bool,
    max_hold: bool,
    min_hold: bool,
    held: Option<Vec<Vec<f64>>>,
    maxima: Option<Vec<Vec<f64>>>,
    minima: Option<Vec<Vec<f64>>>,
    markers: Vec<Marker>,
    next_id: u32,
    column: usize,
    language: Language,
    overlay_visibility: BTreeMap<(String, usize, usize), bool>,
    rendered_overlays: Vec<(String, usize, usize)>,
}

#[derive(Clone, Copy)]
enum Search {
    Maximum,
    Minimum,
    Left,
    Right,
}

impl AnalysisTools {
    /// Called once for each complete measured sweep, never for partial data.
    pub fn observe(&mut self, trace: &SweepData) {
        if !self
            .latest
            .as_ref()
            .is_some_and(|old| same_grid(old, trace))
        {
            self.reset_holds();
            self.column = usize::from(trace.format == "ma");
            for marker in &mut self.markers {
                if let Some(index) = nearest(trace, marker.frequency_hz) {
                    marker.frequency_hz = trace.points[index].freq_hz;
                }
            }
        }
        if self.hold && self.held.is_none() {
            self.held = Some(values(trace));
        }
        update_envelope(&mut self.maxima, self.max_hold, trace, true);
        update_envelope(&mut self.minima, self.min_hold, trace, false);
        self.latest = Some(trace.clone());
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub fn markers_mut(&mut self) -> &mut [Marker] {
        &mut self.markers
    }

    /// Frozen measured rows remain valid complex data. Scalar envelopes
    /// are deliberately excluded from this Smith-chart snapshot.
    pub fn held_trace(&self) -> Option<SweepData> {
        if !self.hold {
            return None;
        }
        let rows = self.held.as_ref()?;
        let mut trace = self.latest.clone()?;
        for (point, row) in trace.points.iter_mut().zip(rows) {
            point.values.clone_from(row);
        }
        Some(trace)
    }

    /// Scalar overlays for the requested raw columns. The caller chooses
    /// columns matching its current display and leaves export data untouched.
    pub fn overlay_series(&mut self, columns: &[usize]) -> Vec<Series<'static>> {
        self.rendered_overlays.clear();
        let Some(trace) = &self.latest else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for (kind, enabled, data, color) in [
            (
                0,
                self.hold,
                &self.held,
                Color32::from_rgb(0x28, 0x99, 0xd0),
            ),
            (
                1,
                self.max_hold,
                &self.maxima,
                Color32::from_rgb(0xe0, 0x65, 0x40),
            ),
            (
                2,
                self.min_hold,
                &self.minima,
                Color32::from_rgb(0x92, 0x68, 0xce),
            ),
        ] {
            let Some(data) = data.as_ref().filter(|_| enabled) else {
                continue;
            };
            for &column in columns {
                let key = (trace.format.clone(), kind, column);
                let visible = self.overlay_visibility.get(&key).copied().unwrap_or(true);
                self.rendered_overlays.push(key);
                result.push(Series {
                    name: overlay_name(kind, &trace.format, column, self.language),
                    color,
                    visible,
                    points: trace
                        .points
                        .iter()
                        .zip(data)
                        .map(|(point, row)| {
                            (point.freq_hz, row.get(column).copied().unwrap_or(f64::NAN))
                        })
                        .collect(),
                });
            }
        }
        result
    }

    /// Apply legend edits to the overlay-only slice returned for this frame.
    /// Keys use wire format, envelope kind and raw column, never translated labels.
    pub fn apply_overlay_visibility(&mut self, series: &[Series<'_>]) {
        for (key, series) in self.rendered_overlays.iter().zip(series) {
            self.overlay_visibility.insert(key.clone(), series.visible);
        }
    }

    pub fn controls(&mut self, ui: &mut egui::Ui, language: Language, trace: Option<&SweepData>) {
        self.controls_for_display(ui, language, trace, false);
    }

    pub fn controls_for_display(
        &mut self,
        ui: &mut egui::Ui,
        language: Language,
        trace: Option<&SweepData>,
        smith: bool,
    ) {
        self.language = language;
        ui.separator();
        ui.vertical_centered(|ui| {
            ui.strong(language.text(Text::AnalysisHold));
        });
        let mut changed = false;
        ui.horizontal(|ui| {
            let width = if smith {
                ui.available_width()
            } else {
                (ui.available_width() - ui.spacing().item_spacing.x * 2.0) / 3.0
            };
            for (index, (flag, text)) in [
                (&mut self.hold, Text::AnalysisHold),
                (&mut self.max_hold, Text::AnalysisMaxHold),
                (&mut self.min_hold, Text::AnalysisMinHold),
            ]
            .into_iter()
            .enumerate()
            {
                if smith && index > 0 {
                    continue;
                }
                if ui
                    .add_sized(
                        [width, 36.0],
                        egui::Button::new(language.text(text)).selected(*flag),
                    )
                    .clicked()
                {
                    *flag = !*flag;
                    changed = true;
                }
            }
        });
        if changed && let Some(trace) = trace {
            if !self.hold {
                self.held = None;
            }
            self.observe(trace);
        }
        if ui
            .small_button(language.text(Text::AnalysisReset))
            .clicked()
        {
            self.reset_holds();
            if let Some(trace) = trace {
                self.observe(trace);
            }
        }
        ui.separator();
        ui.vertical_centered(|ui| {
            ui.strong(language.text(Text::AnalysisMarkers));
        });
        let trace = trace.filter(|t| t.points.iter().any(|p| p.freq_hz.is_finite()));
        self.marker_buttons(ui, language, trace);
        let Some(trace) = trace else {
            ui.small(language.text(Text::NoData));
            return;
        };
        if self.markers.is_empty() {
            return;
        }
        self.marker_actions(ui, language, trace);
        self.marker_readout(ui, language, trace);
        let position = egui::pos2(ui.ctx().content_rect().left() + 316.0, 56.0);
        egui::Window::new(language.text(Text::AnalysisMarkers))
            .id(ui
                .id()
                .with(("marker_table_window", format!("{:?}", trace.mode))))
            .default_pos(position)
            .default_width(290.0)
            .resizable(false)
            .show(ui.ctx(), |ui| self.marker_table(ui, language, trace));
    }

    fn reset_holds(&mut self) {
        self.held = None;
        self.maxima = None;
        self.minima = None;
    }

    fn add_marker(&mut self, trace: &SweepData) {
        if self.markers.len() >= MAX_MARKERS {
            return;
        }
        let Some(point) = trace.points.get(trace.points.len() / 2) else {
            return;
        };
        if !point.freq_hz.is_finite() {
            return;
        }
        for marker in &mut self.markers {
            marker.selected = false;
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.markers.push(Marker {
            id: self.next_id,
            frequency_hz: point.freq_hz,
            selected: true,
            reference: false,
        });
    }

    fn marker_buttons(&mut self, ui: &mut egui::Ui, language: Language, trace: Option<&SweepData>) {
        ui.horizontal(|ui| {
            let width = (ui.available_width() - ui.spacing().item_spacing.x * 2.0) / 3.0;
            let button = |ui: &mut egui::Ui, key, enabled| {
                ui.add_enabled_ui(enabled, |ui| {
                    ui.add_sized([width, 36.0], egui::Button::new(language.text(key)))
                })
                .inner
                .clicked()
            };
            if button(
                ui,
                Text::AnalysisAdd,
                trace.is_some() && self.markers.len() < MAX_MARKERS,
            ) && let Some(trace) = trace
            {
                self.add_marker(trace);
            }
            if button(
                ui,
                Text::AnalysisRemove,
                trace.is_some() && !self.markers.is_empty(),
            ) {
                self.markers.retain(|marker| !marker.selected);
                if let Some(marker) = self.markers.last_mut() {
                    marker.selected = true;
                }
            }
            if button(
                ui,
                Text::AnalysisClear,
                trace.is_some() && !self.markers.is_empty(),
            ) {
                self.markers.clear();
            }
        });
    }

    fn marker_actions(&mut self, ui: &mut egui::Ui, language: Language, trace: &SweepData) {
        let count = trace
            .points
            .iter()
            .map(|p| p.values.len())
            .min()
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        self.column = self.column.min(count - 1);
        ui.horizontal(|ui| {
            ui.label(language.text(Text::AnalysisColumn));
            egui::ComboBox::from_id_salt("marker_column")
                .selected_text(column_label(&trace.format, self.column, language))
                .show_ui(ui, |ui| {
                    for column in 0..count {
                        ui.selectable_value(
                            &mut self.column,
                            column,
                            column_label(&trace.format, column, language),
                        );
                    }
                });
        });
        ui.add_enabled_ui(!self.markers.is_empty(), |ui| {
            ui.horizontal_wrapped(|ui| {
                for (action, text) in [
                    (Search::Maximum, Text::AnalysisMaximum),
                    (Search::Minimum, Text::AnalysisMinimum),
                    (Search::Left, Text::AnalysisLeftPeak),
                    (Search::Right, Text::AnalysisRightPeak),
                ] {
                    if ui.small_button(language.text(text)).clicked() {
                        self.search(trace, action);
                    }
                }
            });
        });
    }

    fn marker_readout(&mut self, ui: &mut egui::Ui, language: Language, trace: &SweepData) {
        let Some(index) = self.markers.iter().position(|marker| marker.selected) else {
            return;
        };
        let mut frequency = self.markers[index].frequency_hz / 1e6;
        ui.horizontal(|ui| {
            ui.label(language.text(Text::AnalysisFrequency));
            if ui
                .add(
                    egui::DragValue::new(&mut frequency)
                        .suffix(" MHz")
                        .speed(0.001)
                        .max_decimals(6),
                )
                .changed()
                && let Some(point) = nearest(trace, frequency * 1e6)
            {
                self.markers[index].frequency_hz = trace.points[point].freq_hz;
            }
        });
        let mut reference = self.markers[index].reference;
        if ui
            .checkbox(&mut reference, language.text(Text::AnalysisDelta))
            .changed()
        {
            for (i, marker) in self.markers.iter_mut().enumerate() {
                marker.reference = i == index && reference;
            }
        }
    }

    fn marker_table(&mut self, ui: &mut egui::Ui, language: Language, trace: &SweepData) {
        let reference = self
            .markers
            .iter()
            .find(|m| m.reference)
            .and_then(|marker| sample(trace, marker.frequency_hz, self.column));
        let mut selected = None;
        egui::Grid::new("analysis_marker_table")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                ui.label("M");
                ui.label("MHz");
                ui.label(language.text(Text::AnalysisValue));
                ui.end_row();
                for marker in self.markers() {
                    let label =
                        format!("M{}{}", marker.id, if marker.reference { " R" } else { "" });
                    if ui.selectable_label(marker.selected, label).clicked() {
                        selected = Some(marker.id);
                    }
                    if let Some((frequency, value)) =
                        sample(trace, marker.frequency_hz, self.column)
                    {
                        let (df, dv) = if marker.reference {
                            (frequency, value)
                        } else if let Some((rf, rv)) = reference {
                            (frequency - rf, value - rv)
                        } else {
                            (frequency, value)
                        };
                        ui.monospace(format!("{:.6}", df / 1e6));
                        ui.monospace(format!("{dv:.4}"));
                    } else {
                        ui.label("-");
                        ui.label("-");
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = selected {
            for marker in &mut self.markers {
                marker.selected = marker.id == id;
            }
        }
    }

    fn search(&mut self, trace: &SweepData, action: Search) {
        let Some(marker) = self.markers.iter_mut().find(|m| m.selected) else {
            return;
        };
        let mut best: Option<(f64, f64)> = None;
        for point in &trace.points {
            let Some(&value) = point.values.get(self.column) else {
                continue;
            };
            if !point.freq_hz.is_finite()
                || !value.is_finite()
                || matches!(action, Search::Left) && point.freq_hz >= marker.frequency_hz
                || matches!(action, Search::Right) && point.freq_hz <= marker.frequency_hz
            {
                continue;
            }
            if best.is_none_or(|(_, previous)| {
                if matches!(action, Search::Minimum) {
                    value < previous
                } else {
                    value > previous
                }
            }) {
                best = Some((point.freq_hz, value));
            }
        }
        if let Some((frequency, _)) = best {
            marker.frequency_hz = frequency;
        }
    }
}

fn same_grid(a: &SweepData, b: &SweepData) -> bool {
    a.mode == b.mode
        && a.format == b.format
        && a.points.len() == b.points.len()
        && a.points
            .iter()
            .zip(&b.points)
            .all(|(a, b)| a.freq_hz == b.freq_hz && a.values.len() == b.values.len())
}

fn values(trace: &SweepData) -> Vec<Vec<f64>> {
    trace
        .points
        .iter()
        .map(|point| point.values.clone())
        .collect()
}

fn update_envelope(
    cache: &mut Option<Vec<Vec<f64>>>,
    enabled: bool,
    trace: &SweepData,
    maximum: bool,
) {
    if !enabled {
        *cache = None;
        return;
    }
    let Some(rows) = cache else {
        *cache = Some(values(trace));
        return;
    };
    for (row, point) in rows.iter_mut().zip(&trace.points) {
        for (old, &new) in row.iter_mut().zip(&point.values) {
            if new.is_finite()
                && (!old.is_finite() || if maximum { new > *old } else { new < *old })
            {
                *old = new;
            }
        }
    }
}

fn nearest(trace: &SweepData, frequency: f64) -> Option<usize> {
    if !frequency.is_finite() {
        return None;
    }
    trace
        .points
        .iter()
        .enumerate()
        .filter(|(_, p)| p.freq_hz.is_finite())
        .min_by(|(_, a), (_, b)| {
            (a.freq_hz - frequency)
                .abs()
                .total_cmp(&(b.freq_hz - frequency).abs())
        })
        .map(|(index, _)| index)
}

fn sample(trace: &SweepData, frequency: f64, column: usize) -> Option<(f64, f64)> {
    let point = &trace.points[nearest(trace, frequency)?];
    let value = *point.values.get(column)?;
    value.is_finite().then_some((point.freq_hz, value))
}

fn column_label(format: &str, column: usize, language: Language) -> &'static str {
    match (format, column) {
        ("ma", 0) => language.text(Text::AnalysisMagnitude),
        ("ma", 1) => language.text(Text::Phase),
        ("ri", 0) => "Re",
        ("ri", 1) => "Im",
        ("z", 0) => "|Z| (ohm)",
        ("z", 1) => "R (ohm)",
        ("z", 2) => "X (ohm)",
        ("loss", _) => "dB",
        ("vswr", _) => "VSWR",
        _ => language.text(Text::Level),
    }
}

fn overlay_name(kind: usize, format: &str, column: usize, language: Language) -> &'static str {
    let names = match (format, column, language) {
        ("z", 0, Language::English) => ["Hold |Z|", "Max |Z|", "Min |Z|"],
        ("z", 1, Language::English) => ["Hold R", "Max R", "Min R"],
        ("z", 2, Language::English) => ["Hold X", "Max X", "Min X"],
        ("z", 0, Language::SimplifiedChinese) => ["保持 |Z|", "最大 |Z|", "最小 |Z|"],
        ("z", 1, Language::SimplifiedChinese) => ["保持 R", "最大 R", "最小 R"],
        ("z", 2, Language::SimplifiedChinese) => ["保持 X", "最大 X", "最小 X"],
        _ => [
            language.text(Text::AnalysisHold),
            language.text(Text::AnalysisMaxHold),
            language.text(Text::AnalysisMinHold),
        ],
    };
    names[kind]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcsdi_core::{data::SweepPoint, protocol::StreamMode};

    fn trace(values: &[f64]) -> SweepData {
        SweepData {
            mode: StreamMode::Spec,
            format: String::new(),
            points: values
                .iter()
                .enumerate()
                .map(|(index, &value)| SweepPoint {
                    freq_hz: (index + 1) as f64 * 1e6,
                    values: vec![value],
                })
                .collect(),
        }
    }

    #[test]
    fn holds_preserve_snapshot_and_update_envelopes_without_mutating_input() {
        let first = trace(&[-8.0, -5.0, -10.0]);
        let second = trace(&[-4.0, -9.0, f64::NAN]);
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&first);
        tools.observe(&second);
        assert_eq!(tools.held, Some(vec![vec![-8.0], vec![-5.0], vec![-10.0]]));
        assert_eq!(
            tools.maxima,
            Some(vec![vec![-4.0], vec![-5.0], vec![-10.0]])
        );
        assert_eq!(
            tools.minima,
            Some(vec![vec![-8.0], vec![-9.0], vec![-10.0]])
        );
        assert_eq!(first.points[0].values, vec![-8.0]);
        assert_eq!(tools.overlay_series(&[0]).len(), 3);
    }

    #[test]
    fn overlay_visibility_survives_frames_reordering_translation_and_new_sweeps() {
        let mut data = trace(&[3.0, 4.0, 5.0]);
        data.mode = StreamMode::S11;
        data.format = "z".into();
        for point in &mut data.points {
            point.values = vec![50.0, 30.0, 40.0];
        }
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            ..Default::default()
        };
        tools.observe(&data);
        let mut overlays = tools.overlay_series(&[0, 1, 2]);
        overlays[1].visible = false;
        tools.apply_overlay_visibility(&overlays);
        tools.language = Language::SimplifiedChinese;
        tools.observe(&data);
        let reordered = tools.overlay_series(&[2, 1]);
        assert!(reordered[0].visible);
        assert!(!reordered[1].visible);
        assert!(reordered[2].visible && reordered[3].visible);
        assert_eq!(reordered[1].points[0], (1e6, 30.0));
    }

    #[test]
    fn marker_table_is_outside_the_parameter_panel_clip_and_clear_of_the_legend() {
        let ctx = egui::Context::default();
        let data = trace(&[3.0, 4.0, 5.0]);
        let mut tools = AnalysisTools::default();
        tools.observe(&data);
        tools.add_marker(&data);
        let mut visible = false;
        for frame in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 850.0),
                    )),
                    time: Some(frame as f64 / 60.0),
                    ..Default::default()
                },
                |ui| {
                    egui::Panel::left("traces")
                        .exact_size(256.0)
                        .show(ui, |_| {});
                    egui::Panel::right("parameters")
                        .exact_size(256.0)
                        .show(ui, |ui| tools.controls(ui, Language::English, Some(&data)));
                },
            );
            visible |= output.shapes.iter().any(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == "M1" => {
                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                    bounds.left() >= 316.0
                        && bounds.right() < 620.0
                        && shape.clip_rect.right() < 840.0
                        && shape.clip_rect.contains_rect(bounds)
                }
                _ => false,
            });
            output.drop_without_applying_deltas();
        }
        assert!(
            visible,
            "the marker row must be drawn in its own floating window"
        );
    }

    #[test]
    fn grid_format_and_column_changes_reset_envelopes() {
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            ..Default::default()
        };
        let mut data = trace(&[1.0, 2.0, 3.0]);
        tools.observe(&data);
        data.points[0].freq_hz += 1.0;
        data.points[0].values[0] = -1.0;
        tools.observe(&data);
        assert_eq!(tools.maxima.as_ref().unwrap()[0][0], -1.0);
        data.format = "ma".into();
        for point in &mut data.points {
            point.values = vec![0.5, -30.0];
        }
        tools.observe(&data);
        assert_eq!(tools.column, 1);
        assert_eq!(tools.held.as_ref().unwrap()[0], vec![0.5, -30.0]);
    }

    #[test]
    fn marker_search_uses_measured_extrema_and_keeps_ids() {
        let data = trace(&[9.0, 4.0, 3.0, 8.0, 5.0]);
        let mut tools = AnalysisTools::default();
        tools.add_marker(&data);
        tools.search(&data, Search::Left);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        tools.search(&data, Search::Right);
        assert_eq!(tools.markers[0].frequency_hz, 4e6);
        tools.search(&data, Search::Minimum);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        for _ in 0..20 {
            tools.add_marker(&data);
        }
        assert_eq!(tools.markers.len(), MAX_MARKERS);
        assert_eq!(tools.markers.iter().filter(|m| m.selected).count(), 1);
        assert_eq!(sample(&data, 2.4e6, 0), Some((2e6, 4.0)));
    }

    #[test]
    fn smith_hold_retains_whole_measured_rows_instead_of_scalar_envelopes() {
        let first = SweepData {
            mode: StreamMode::S11,
            format: "z".into(),
            points: vec![SweepPoint {
                freq_hz: 1e6,
                values: vec![50.0, 30.0, 40.0],
            }],
        };
        let second = SweepData {
            mode: StreamMode::S11,
            format: "z".into(),
            points: vec![SweepPoint {
                freq_hz: 1e6,
                values: vec![50.0, 50.0, 0.0],
            }],
        };
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            ..Default::default()
        };
        tools.observe(&first);
        tools.observe(&second);
        assert_eq!(tools.held_trace(), Some(first));
        assert_eq!(tools.maxima.as_ref().unwrap()[0], vec![50.0, 50.0, 40.0]);
    }
}
