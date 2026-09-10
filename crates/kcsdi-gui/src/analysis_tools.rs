// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Session analysis of completed sweeps. Scalar envelopes never become
//! complex acquisition data or replace the snapshot used for export.

use egui::Color32;
use kcsdi_core::data::SweepData;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::acquisition::CompletedSweep;
use crate::i18n::{Language, Text};
use crate::widgets::plot::{Marker, Series, format_value};

const MAX_MARKERS: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerTarget {
    Hold,
    Maximum,
    Minimum,
    #[default]
    #[serde(other)]
    Current,
}

impl MarkerTarget {
    fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Current => Text::AnalysisCurrent,
            Self::Hold => Text::AnalysisHold,
            Self::Maximum => Text::AnalysisMaxHold,
            Self::Minimum => Text::AnalysisMinHold,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayVisibilityConfig {
    pub format: String,
    pub kind: usize,
    pub column: usize,
    pub visible: bool,
}

/// User settings only. Measurement rows and hold buffers are session data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisConfig {
    pub markers: Vec<Marker>,
    pub next_id: u32,
    pub hold: bool,
    pub max_hold: bool,
    pub min_hold: bool,
    pub column: usize,
    pub target: MarkerTarget,
    pub overlay_visibility: Vec<OverlayVisibilityConfig>,
}

#[derive(Default)]
pub struct AnalysisTools {
    latest: Option<CompletedSweep>,
    hold: bool,
    max_hold: bool,
    min_hold: bool,
    held: Option<Vec<Vec<f64>>>,
    maxima: Option<Vec<Vec<f64>>>,
    minima: Option<Vec<Vec<f64>>>,
    markers: Vec<Marker>,
    next_id: u32,
    trace_id: u64,
    column: usize,
    fixed_column: Option<usize>,
    restored_column: bool,
    target: MarkerTarget,
    smith: bool,
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

enum TargetRows<'a> {
    Current(&'a SweepData),
    Stored(&'a [Vec<f64>]),
}

impl TargetRows<'_> {
    fn get(&self, index: usize) -> Option<&[f64]> {
        match self {
            Self::Current(trace) => Some(&trace.points.get(index)?.values),
            Self::Stored(rows) => Some(rows.get(index)?.as_slice()),
        }
    }
}

impl AnalysisTools {
    /// Keep definitions, but never combine data across a calibration write.
    pub fn invalidate_measurements(&mut self) {
        self.latest = None;
        self.reset_holds();
        self.rendered_overlays.clear();
        self.restored_column = true;
    }

    pub fn config(&self) -> AnalysisConfig {
        AnalysisConfig {
            markers: self.markers.clone(),
            next_id: self.next_id,
            hold: self.hold,
            max_hold: self.max_hold,
            min_hold: self.min_hold,
            column: self.column,
            target: self.target,
            overlay_visibility: self
                .overlay_visibility
                .iter()
                .map(
                    |((format, kind, column), &visible)| OverlayVisibilityConfig {
                        format: format.clone(),
                        kind: *kind,
                        column: *column,
                        visible,
                    },
                )
                .collect(),
        }
    }

    pub fn restore_config(&mut self, config: &AnalysisConfig) {
        self.latest = None;
        self.reset_holds();
        self.rendered_overlays.clear();
        let mut ids = BTreeSet::new();
        self.markers = config
            .markers
            .iter()
            .filter(|marker| {
                marker.id != 0
                    && marker.frequency_hz.is_finite()
                    && marker.frequency_hz >= 0.0
                    && ids.insert(marker.id)
            })
            .take(MAX_MARKERS)
            .cloned()
            .collect();
        let selected = self
            .markers
            .iter()
            .position(|marker| marker.selected)
            .unwrap_or(0);
        let reference = self.markers.iter().position(|marker| marker.reference);
        for (index, marker) in self.markers.iter_mut().enumerate() {
            marker.selected = index == selected;
            marker.reference = Some(index) == reference;
        }
        self.next_id = config.next_id.max(
            self.markers
                .iter()
                .map(|marker| marker.id)
                .max()
                .unwrap_or(0),
        );
        self.hold = config.hold;
        self.max_hold = config.max_hold;
        self.min_hold = config.min_hold;
        self.column = self.fixed_column.unwrap_or(config.column.min(2));
        self.restored_column = true;
        self.target = config.target;
        self.overlay_visibility = config
            .overlay_visibility
            .iter()
            .filter(|entry| {
                entry.kind < 3
                    && entry.column < 3
                    && ["", "ri", "ma", "z", "loss", "vswr", "delay"]
                        .contains(&entry.format.as_str())
            })
            .map(|entry| {
                (
                    (entry.format.clone(), entry.kind, entry.column),
                    entry.visible,
                )
            })
            .collect();
        self.set_smith(self.smith);
    }

    /// Called once for each complete measured sweep, never for partial data.
    pub fn observe(&mut self, snapshot: &CompletedSweep) {
        let trace = &snapshot.data;
        if !self.latest.as_ref().is_some_and(|old| {
            old.session_id == snapshot.session_id
                && old.settings == snapshot.settings
                && same_grid(&old.data, trace)
        }) {
            self.reset_holds();
            if self.latest.as_ref().map_or(!self.restored_column, |old| {
                old.data.mode != trace.mode || old.data.format != trace.format
            }) {
                self.column = self
                    .fixed_column
                    .unwrap_or_else(|| usize::from(trace.format == "ma"));
            }
            for marker in &mut self.markers {
                if let Some(index) = nearest(trace, marker.frequency_hz) {
                    marker.frequency_hz = trace.points[index].freq_hz;
                }
            }
        }
        self.refresh_holds(trace);
        self.latest = Some(snapshot.clone());
        self.restored_column = false;
        self.refresh_auto_peaks(trace);
    }

    fn refresh_auto_peaks(&mut self, trace: &SweepData) {
        if !self.smith {
            let peak = self.extremum(trace, Search::Maximum, 0.0);
            if let Some(frequency) = peak {
                for marker in &mut self.markers {
                    if marker.auto_peak {
                        marker.frequency_hz = frequency;
                    }
                }
            }
        }
    }

    fn refresh_holds(&mut self, trace: &SweepData) {
        if !self.hold {
            self.held = None;
        } else if self.held.is_none() {
            self.held = Some(values(trace));
        }
        update_envelope(&mut self.maxima, self.max_hold, trace, true);
        update_envelope(&mut self.minima, self.min_hold, trace, false);
    }

    pub fn set_trace_id(&mut self, trace_id: u64) {
        self.trace_id = trace_id;
    }

    pub fn set_column(&mut self, column: Option<usize>) {
        self.fixed_column = column;
        if let Some(column) = column {
            self.column = column;
        }
    }

    pub fn set_language(&mut self, language: Language) {
        self.language = language;
    }

    pub fn set_smith(&mut self, smith: bool) {
        self.smith = smith;
        if smith {
            if matches!(self.target, MarkerTarget::Maximum | MarkerTarget::Minimum) {
                self.target = MarkerTarget::Current;
            }
            for marker in &mut self.markers {
                marker.auto_peak = false;
                marker.reference = false;
            }
        }
    }

    /// Smith marker positions use measured rows from the explicit target.
    /// Scalar envelopes are not complex measurements.
    pub fn marker_trace(&self) -> Option<SweepData> {
        match self.target {
            MarkerTarget::Current => Some(self.latest.as_ref()?.data.clone()),
            MarkerTarget::Hold => self.held_trace(),
            MarkerTarget::Maximum | MarkerTarget::Minimum => None,
        }
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub fn markers_mut(&mut self) -> &mut [Marker] {
        &mut self.markers
    }

    /// Scalar marker interaction uses the completed target, never a preview.
    pub fn marker_frequencies(&self, trace: &SweepData) -> Vec<f64> {
        trace
            .points
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                let value = self.target_row(trace, index)?.get(self.column)?;
                (point.freq_hz.is_finite() && value.is_finite()).then_some(point.freq_hz)
            })
            .collect()
    }

    /// Frozen measured rows remain valid complex data. Scalar envelopes
    /// are deliberately excluded from this Smith-chart snapshot.
    pub fn held_trace(&self) -> Option<SweepData> {
        if !self.hold {
            return None;
        }
        let rows = self.held.as_ref()?;
        let mut trace = self.latest.as_ref()?.data.clone();
        for (point, row) in trace.points.iter_mut().zip(rows) {
            point.values.clone_from(row);
        }
        Some(trace)
    }

    /// Scalar overlays for the requested raw columns. The caller chooses
    /// columns matching its current display and leaves export data untouched.
    pub fn overlay_series(&mut self, columns: &[usize]) -> Vec<Series<'static>> {
        self.rendered_overlays.clear();
        let Some(snapshot) = &self.latest else {
            return Vec::new();
        };
        let trace = &snapshot.data;
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

    pub fn controls_for_display(
        &mut self,
        ui: &mut egui::Ui,
        language: Language,
        trace: Option<&CompletedSweep>,
        smith: bool,
        sweep_center_hz: f64,
        allow_sweep_center: bool,
    ) -> Option<f64> {
        self.set_language(language);
        self.set_smith(smith);
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
        if changed {
            if !self.hold {
                self.held = None;
            }
            if !self.max_hold {
                self.maxima = None;
            }
            if !self.min_hold {
                self.minima = None;
            }
            if let Some(trace) = trace {
                self.refresh_holds(&trace.data);
                self.refresh_auto_peaks(&trace.data);
            }
        }
        if ui
            .small_button(language.text(Text::AnalysisReset))
            .clicked()
        {
            self.reset_holds();
            if let Some(trace) = trace {
                self.refresh_holds(&trace.data);
                self.refresh_auto_peaks(&trace.data);
            }
        }
        ui.separator();
        ui.vertical_centered(|ui| {
            ui.strong(language.text(Text::AnalysisMarkers));
        });
        let trace = trace
            .map(|snapshot| &snapshot.data)
            .filter(|t| t.points.iter().any(|p| p.freq_hz.is_finite()));
        let previous_target = self.target;
        self.target_control(ui, language);
        self.marker_buttons(ui, language, trace, sweep_center_hz);
        let Some(trace) = trace else {
            ui.small(language.text(Text::NoData));
            return None;
        };
        if self.target != previous_target {
            self.refresh_auto_peaks(trace);
        }
        if self.markers.is_empty() {
            return None;
        }
        let ready = self.target_row(trace, 0).is_some();
        if !ready {
            ui.small(language.text(Text::AnalysisTargetUnavailable));
        }
        ui.add_enabled_ui(ready, |ui| {
            if !smith {
                self.marker_actions(ui, language, trace);
            }
            if ui
                .small_button(language.text(Text::AnalysisCenter))
                .clicked()
            {
                self.move_selected(trace, sweep_center_hz);
            }
        });
        let center = self.marker_readout(ui, language, trace, ready, allow_sweep_center);
        let position = egui::pos2(ui.ctx().content_rect().left() + 316.0, 56.0);
        egui::Window::new(format!(
            "T{} {}",
            self.trace_id,
            language.text(Text::AnalysisMarkers)
        ))
        .id(ui.id().with(("marker_table_window", self.trace_id)))
        .default_pos(position)
        .default_width(290.0)
        .resizable(false)
        .show(ui.ctx(), |ui| self.marker_table(ui, language, trace));
        center
    }

    fn target_control(&mut self, ui: &mut egui::Ui, language: Language) {
        ui.horizontal(|ui| {
            ui.label(language.text(Text::AnalysisTarget));
            egui::ComboBox::from_id_salt("marker_target")
                .width(ui.available_width())
                .selected_text(self.target.label(language))
                .show_ui(ui, |ui| {
                    for target in [
                        MarkerTarget::Current,
                        MarkerTarget::Hold,
                        MarkerTarget::Maximum,
                        MarkerTarget::Minimum,
                    ] {
                        if self.smith
                            && matches!(target, MarkerTarget::Maximum | MarkerTarget::Minimum)
                        {
                            continue;
                        }
                        ui.selectable_value(&mut self.target, target, target.label(language));
                    }
                });
        });
    }

    fn reset_holds(&mut self) {
        self.held = None;
        self.maxima = None;
        self.minima = None;
    }

    fn add_marker(&mut self, trace: &SweepData, center_hz: f64) {
        if self.markers.len() >= MAX_MARKERS {
            return;
        }
        let Some(index) = nearest(trace, center_hz) else {
            return;
        };
        let Some(id) = self.next_id.checked_add(1) else {
            return;
        };
        for marker in &mut self.markers {
            marker.selected = false;
        }
        self.next_id = id;
        self.markers.push(Marker {
            id,
            frequency_hz: trace.points[index].freq_hz,
            selected: true,
            reference: false,
            auto_peak: false,
        });
    }

    fn marker_buttons(
        &mut self,
        ui: &mut egui::Ui,
        language: Language,
        trace: Option<&SweepData>,
        center_hz: f64,
    ) {
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
                trace.is_some() && self.markers.len() < MAX_MARKERS && self.next_id < u32::MAX,
            ) && let Some(trace) = trace
            {
                self.add_marker(trace, center_hz);
            }
            if button(ui, Text::AnalysisRemove, !self.markers.is_empty()) {
                self.markers.retain(|marker| !marker.selected);
                if let Some(marker) = self.markers.last_mut() {
                    marker.selected = true;
                }
            }
            if button(ui, Text::AnalysisClear, !self.markers.is_empty()) {
                self.markers.clear();
            }
        });
    }

    fn marker_actions(&mut self, ui: &mut egui::Ui, language: Language, trace: &SweepData) {
        let previous_column = self.column;
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
        if self.fixed_column.is_none() {
            ui.horizontal(|ui| {
                ui.label(language.text(Text::AnalysisColumn));
                egui::ComboBox::from_id_salt("marker_column")
                    .width(ui.available_width())
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
        }
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
            let changed = self
                .markers
                .iter_mut()
                .find(|marker| marker.selected)
                .is_some_and(|marker| {
                    ui.checkbox(&mut marker.auto_peak, language.text(Text::AnalysisAutoPeak))
                        .changed()
                });
            if changed || self.column != previous_column {
                self.refresh_auto_peaks(trace);
            }
        });
    }

    fn marker_readout(
        &mut self,
        ui: &mut egui::Ui,
        language: Language,
        trace: &SweepData,
        ready: bool,
        allow_sweep_center: bool,
    ) -> Option<f64> {
        let index = self.markers.iter().position(|marker| marker.selected)?;
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
            {
                self.move_selected(trace, frequency * 1e6);
            }
        });
        let mut center = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    ready && allow_sweep_center,
                    egui::Button::new(language.text(Text::AnalysisCenterAtMarker)),
                )
                .on_hover_text(language.text(Text::AnalysisCenterAtMarkerHelp))
                .clicked()
            {
                center = Some(self.markers[index].frequency_hz);
            }
            if !self.smith {
                let mut reference = self.markers[index].reference;
                if ui
                    .checkbox(&mut reference, language.text(Text::AnalysisDeltaReference))
                    .changed()
                {
                    for (i, marker) in self.markers.iter_mut().enumerate() {
                        marker.reference = i == index && reference;
                    }
                }
            }
        });
        center
    }

    fn marker_table(&mut self, ui: &mut egui::Ui, language: Language, trace: &SweepData) {
        ui.label(format!(
            "{}: {}",
            language.text(Text::AnalysisTarget),
            self.target.label(language)
        ));
        if !self.smith {
            ui.label(format!(
                "{}: {}",
                language.text(Text::AnalysisColumn),
                column_label(&trace.format, self.column, language)
            ));
        }
        let reference = (!self.smith)
            .then(|| self.markers.iter().find(|marker| marker.reference))
            .flatten();
        let reference_value = reference
            .and_then(|marker| self.target_sample(trace, marker.frequency_hz, self.column));
        let mut selected = None;
        egui::Grid::new("analysis_marker_table")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.label("M");
                ui.label("MHz");
                ui.label(if self.smith {
                    "R (ohm)"
                } else {
                    language.text(Text::AnalysisValue)
                });
                ui.label(if self.smith { "X (ohm)" } else { "" });
                ui.end_row();
                for marker in self.markers() {
                    let is_reference = reference.is_some_and(|reference| reference.id == marker.id);
                    let delta = reference.is_some() && !is_reference;
                    let suffix = if is_reference {
                        language.text(Text::AnalysisReference)
                    } else if delta {
                        language.text(Text::AnalysisDelta)
                    } else {
                        ""
                    };
                    let label = format!("M{} {suffix}", marker.id).trim_end().to_owned();
                    if ui.selectable_label(marker.selected, label).clicked() {
                        selected = Some(marker.id);
                    }
                    if self.smith {
                        if let Some((frequency, resistance, reactance)) =
                            self.smith_sample(trace, marker.frequency_hz)
                        {
                            ui.monospace(format!("{:.6}", frequency / 1e6));
                            ui.monospace(format_value(resistance));
                            ui.monospace(format_value(reactance));
                        } else {
                            for _ in 0..3 {
                                ui.label("-");
                            }
                        }
                    } else {
                        let value = self.target_sample(trace, marker.frequency_hz, self.column);
                        let value = if delta {
                            value.zip(reference_value).and_then(|((f, v), (rf, rv))| {
                                let pair = (f - rf, v - rv);
                                (pair.0.is_finite() && pair.1.is_finite()).then_some(pair)
                            })
                        } else {
                            value
                        };
                        if let Some((frequency, value)) = value {
                            ui.monospace(format!("{:.6}", frequency / 1e6));
                            ui.monospace(format_value(value));
                        } else {
                            ui.label("-");
                            ui.label("-");
                        }
                        ui.label(difference_unit(trace, self.column, delta));
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
        let Some(marker) = self.markers.iter().find(|marker| marker.selected) else {
            return;
        };
        let frequency = self.extremum(trace, action, marker.frequency_hz);
        if let Some(marker) = self.markers.iter_mut().find(|marker| marker.selected) {
            marker.auto_peak = false;
            if let Some(frequency) = frequency {
                marker.frequency_hz = frequency;
            }
        }
    }

    fn move_selected(&mut self, trace: &SweepData, frequency: f64) {
        let Some(marker) = self.markers.iter_mut().find(|marker| marker.selected) else {
            return;
        };
        marker.auto_peak = false;
        if let Some(index) = nearest(trace, frequency) {
            marker.frequency_hz = trace.points[index].freq_hz;
        }
    }

    fn extremum(&self, trace: &SweepData, action: Search, frequency: f64) -> Option<f64> {
        let rows = self.target_rows(trace)?;
        let mut best: Option<(f64, f64)> = None;
        for (index, point) in trace.points.iter().enumerate() {
            let Some(&value) = rows.get(index).and_then(|row| row.get(self.column)) else {
                continue;
            };
            if !point.freq_hz.is_finite()
                || !value.is_finite()
                || matches!(action, Search::Left) && point.freq_hz >= frequency
                || matches!(action, Search::Right) && point.freq_hz <= frequency
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
        best.map(|(frequency, _)| frequency)
    }

    fn target_rows<'a>(&'a self, trace: &'a SweepData) -> Option<TargetRows<'a>> {
        if self.target == MarkerTarget::Current {
            return Some(TargetRows::Current(trace));
        }
        if !self
            .latest
            .as_ref()
            .is_some_and(|snapshot| same_grid(&snapshot.data, trace))
        {
            return None;
        }
        let rows = match self.target {
            MarkerTarget::Hold if self.hold => self.held.as_ref(),
            MarkerTarget::Maximum if self.max_hold && !self.smith => self.maxima.as_ref(),
            MarkerTarget::Minimum if self.min_hold && !self.smith => self.minima.as_ref(),
            _ => None,
        }?;
        Some(TargetRows::Stored(rows.as_slice()))
    }

    fn target_row<'a>(&'a self, trace: &'a SweepData, index: usize) -> Option<&'a [f64]> {
        match self.target_rows(trace)? {
            TargetRows::Current(trace) => Some(&trace.points.get(index)?.values),
            TargetRows::Stored(rows) => Some(rows.get(index)?.as_slice()),
        }
    }

    fn target_sample(
        &self,
        trace: &SweepData,
        frequency: f64,
        column: usize,
    ) -> Option<(f64, f64)> {
        let index = nearest(trace, frequency)?;
        let value = *self.target_row(trace, index)?.get(column)?;
        value
            .is_finite()
            .then_some((trace.points[index].freq_hz, value))
    }

    fn smith_sample(&self, trace: &SweepData, frequency: f64) -> Option<(f64, f64, f64)> {
        let index = nearest(trace, frequency)?;
        let row = self.target_row(trace, index)?;
        let (resistance, reactance) = match (trace.format.as_str(), row) {
            ("z", [_, resistance, reactance]) => (*resistance, *reactance),
            ("ri", [real, imag]) => {
                let denominator = (1.0 - real).powi(2) + imag.powi(2);
                (
                    50.0 * (1.0 - real.powi(2) - imag.powi(2)) / denominator,
                    100.0 * imag / denominator,
                )
            }
            _ => return None,
        };
        (resistance.is_finite() && reactance.is_finite()).then_some((
            trace.points[index].freq_hz,
            resistance,
            reactance,
        ))
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
        ("delay", _) => "s",
        ("vswr", _) => "VSWR",
        _ => language.text(Text::Level),
    }
}

fn column_unit(trace: &SweepData, column: usize) -> &'static str {
    match (trace.format.as_str(), column) {
        ("ma", 1) => "deg",
        ("z", _) => "ohm",
        ("loss", _) => "dB",
        ("delay", _) => "s",
        ("", _) if trace.mode == kcsdi_core::protocol::StreamMode::Spec => "dBm",
        _ => "",
    }
}

fn difference_unit(trace: &SweepData, column: usize, difference: bool) -> &'static str {
    match (column_unit(trace, column), difference) {
        ("dBm", true) => "dB",
        (unit, _) => unit,
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

    fn snapshot(data: &SweepData) -> CompletedSweep {
        use crate::acquisition::{AcquisitionSettings, tests};
        let mut settings = if data.mode == StreamMode::S11 {
            let mut params = tests::s11();
            params.format = data.format.parse().unwrap();
            params.points = data.points.len() as u32;
            AcquisitionSettings::S11(params)
        } else if data.mode == StreamMode::S21 {
            AcquisitionSettings::S21(kcsdi_core::device::S21Params {
                cal: kcsdi_core::commands::Cal::CalOff,
                format: data.format.parse().unwrap(),
                lo: kcsdi_core::commands::Lo::HighLo,
                points: data.points.len() as u32,
                start_hz: 1_000_000,
                stop_hz: 2_000_000,
                rbw: Some(kcsdi_core::model::Rbw::R10k),
            })
        } else {
            let mut params = tests::spec();
            params.points = data.points.len() as u32;
            AcquisitionSettings::Spec(params)
        };
        let (start_hz, stop_hz) = (
            data.points[0].freq_hz as u64,
            data.points.last().unwrap().freq_hz as u64,
        );
        match &mut settings {
            AcquisitionSettings::S11(params) => {
                params.start_hz = start_hz;
                params.stop_hz = stop_hz;
            }
            AcquisitionSettings::S21(params) => {
                params.start_hz = start_hz;
                params.stop_hz = stop_hz;
            }
            AcquisitionSettings::Spec(params) => {
                params.start_hz = start_hz;
                params.stop_hz = stop_hz;
            }
            AcquisitionSettings::List { .. } => unreachable!("finite snapshot fixture"),
        }
        assert!(settings.accepts(data));
        CompletedSweep {
            data: data.clone(),
            settings,
            session_id: 1,
            completed_at: std::time::SystemTime::UNIX_EPOCH,
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
        tools.observe(&snapshot(&first));
        tools.observe(&snapshot(&second));
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
        tools.observe(&snapshot(&data));
        let mut overlays = tools.overlay_series(&[0, 1, 2]);
        overlays[1].visible = false;
        tools.apply_overlay_visibility(&overlays);
        tools.set_language(Language::SimplifiedChinese);
        tools.observe(&snapshot(&data));
        let reordered = tools.overlay_series(&[2, 1]);
        assert!(reordered[0].visible);
        assert!(!reordered[1].visible);
        assert!(reordered[2].visible && reordered[3].visible);
        assert_eq!(reordered[1].points[0], (1e6, 30.0));
    }

    #[test]
    fn overlay_labels_follow_language_without_opening_trace_controls() {
        let complete = snapshot(&trace(&[3.0, 4.0, 5.0]));
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&complete);
        let config = tools.config();
        for language in [Language::SimplifiedChinese, Language::English] {
            tools.set_language(language);
            let overlays = tools.overlay_series(&[0]);
            assert_eq!(
                overlays
                    .iter()
                    .map(|series| series.name)
                    .collect::<Vec<_>>(),
                [
                    Text::AnalysisHold,
                    Text::AnalysisMaxHold,
                    Text::AnalysisMinHold
                ]
                .map(|key| language.text(key))
            );
            assert!(overlays.iter().all(|series| series.points[0] == (1e6, 3.0)));
            assert_eq!(tools.config(), config);
        }
        assert_eq!(tools.latest.as_ref().unwrap().data, complete.data);
    }

    #[test]
    fn marker_table_is_outside_the_parameter_panel_clip_and_clear_of_the_legend() {
        let ctx = egui::Context::default();
        let data = trace(&[3.0, 4.0, 5.0]);
        let mut tools = AnalysisTools::default();
        let completed = snapshot(&data);
        tools.observe(&completed);
        tools.add_marker(&data, 2e6);
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
                        .show(ui, |ui| {
                            tools.controls_for_display(
                                ui,
                                Language::English,
                                Some(&completed),
                                false,
                                2e6,
                                true,
                            )
                        });
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
    fn marker_table_keeps_small_delay_values_and_deltas_in_seconds() {
        let mut data = trace(&[-5.147039e-9, 0.375e-9]);
        data.mode = StreamMode::S21;
        data.format = "delay".into();
        let mut tools = AnalysisTools::default();
        tools.add_marker(&data, 2e6);
        tools.markers[0].frequency_hz = 1e6;
        tools.markers[0].reference = true;
        tools.add_marker(&data, 2e6);
        for language in [Language::English, Language::SimplifiedChinese] {
            let ctx = egui::Context::default();
            let output = ctx.run_ui(Default::default(), |ui| {
                tools.marker_table(ui, language, &data);
            });
            let text: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect();
            assert!(text.contains(&language.text(Text::AnalysisValue)));
            assert!(text.contains(&"s"));
            assert!(text.contains(&"-5.147e-9"));
            assert!(text.contains(&"5.522e-9"));
            assert!(!text.contains(&"0.0000") && !text.contains(&"-0.0000"));
            output.drop_without_applying_deltas();
        }
        assert_eq!(data.points[0].values[0], -5.147039e-9);
        assert_eq!(column_label("delay", 0, Language::English), "s");
    }

    #[test]
    fn marker_units_match_the_raw_measurement_column() {
        let mut data = trace(&[1.0, 2.0]);
        assert_eq!(column_unit(&data, 0), "dBm");
        data.mode = StreamMode::S21;
        for (format, column, unit) in [
            ("ma", 0, ""),
            ("ma", 1, "deg"),
            ("loss", 0, "dB"),
            ("delay", 0, "s"),
            ("ri", 0, ""),
            ("ri", 1, ""),
        ] {
            data.format = format.into();
            assert_eq!(column_unit(&data, column), unit);
        }
        data.mode = StreamMode::S11;
        data.format = "z".into();
        for column in 0..3 {
            assert_eq!(column_unit(&data, column), "ohm");
        }
    }

    #[test]
    fn s21_holds_keep_raw_seconds_and_reset_for_calibration_or_lo_changes() {
        use crate::acquisition::AcquisitionSettings;
        use kcsdi_core::commands::{Cal, Lo};

        let delay = |values: &[f64]| {
            let mut data = trace(values);
            data.mode = StreamMode::S21;
            data.format = "delay".into();
            snapshot(&data)
        };
        let first = delay(&[-5e-9, 2e-9, -3e-9]);
        let second = delay(&[-4e-9, 1e-9, -8e-9]);
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&first);
        tools.observe(&second);
        let overlays = tools.overlay_series(&[0]);
        assert_eq!(overlays[0].points[0], (1e6, -5e-9));
        assert_eq!(overlays[1].points[0], (1e6, -4e-9));
        assert_eq!(overlays[2].points[2], (3e6, -8e-9));
        for (cal, lo) in [(Cal::CalSys, Lo::HighLo), (Cal::CalOff, Lo::LowLo)] {
            let mut changed = delay(&[1e-9, 2e-9, 3e-9]);
            let AcquisitionSettings::S21(params) = &mut changed.settings else {
                unreachable!()
            };
            params.cal = cal;
            params.lo = lo;
            tools.observe(&changed);
            assert!(
                tools
                    .overlay_series(&[0])
                    .iter()
                    .all(|series| series.points[0] == (1e6, 1e-9))
            );
        }
        assert_eq!(first.data.points[0].values[0], -5e-9);
    }

    #[test]
    fn s21_phase_markers_search_phase_instead_of_magnitude() {
        let mut data = trace(&[0.9, 0.1, 0.2]);
        data.mode = StreamMode::S21;
        data.format = "ma".into();
        for (point, phase) in data.points.iter_mut().zip([-80.0, 120.0, -30.0]) {
            point.values.push(phase);
        }
        let mut tools = AnalysisTools::default();
        tools.observe(&snapshot(&data));
        assert_eq!(tools.column, 1);
        tools.add_marker(&data, 2e6);
        tools.search(&data, Search::Maximum);
        assert_eq!(tools.markers[0].frequency_hz, 2e6);
        tools.search(&data, Search::Minimum);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
    }

    #[test]
    fn grid_format_and_column_changes_reset_envelopes() {
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            ..Default::default()
        };
        let mut data = trace(&[1.0, 2.0, 3.0]);
        tools.observe(&snapshot(&data));
        data.points[0].freq_hz += 1.0;
        data.points[0].values[0] = -1.0;
        tools.observe(&snapshot(&data));
        assert_eq!(tools.maxima.as_ref().unwrap()[0][0], -1.0);
        data.format = "ma".into();
        data.mode = StreamMode::S11;
        for point in &mut data.points {
            point.values = vec![0.5, -30.0];
        }
        tools.observe(&snapshot(&data));
        assert_eq!(tools.column, 1);
        assert_eq!(tools.held.as_ref().unwrap()[0], vec![0.5, -30.0]);
    }

    #[test]
    fn marker_search_uses_measured_extrema_and_keeps_ids() {
        let data = trace(&[9.0, 4.0, 3.0, 8.0, 5.0]);
        let mut tools = AnalysisTools::default();
        tools.add_marker(&data, 3e6);
        tools.search(&data, Search::Left);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        tools.search(&data, Search::Right);
        assert_eq!(tools.markers[0].frequency_hz, 4e6);
        tools.search(&data, Search::Minimum);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        for _ in 0..20 {
            tools.add_marker(&data, 3e6);
        }
        assert_eq!(tools.markers.len(), MAX_MARKERS);
        assert_eq!(tools.markers.iter().filter(|m| m.selected).count(), 1);
        assert_eq!(tools.target_sample(&data, 2.4e6, 0), Some((2e6, 4.0)));
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
        tools.observe(&snapshot(&first));
        tools.observe(&snapshot(&second));
        assert_eq!(tools.held_trace(), Some(first));
        assert_eq!(tools.maxima.as_ref().unwrap()[0], vec![50.0, 50.0, 40.0]);
    }

    #[test]
    fn acquisition_conditions_and_sessions_reset_holds_on_an_identical_grid() {
        use crate::acquisition::AcquisitionSettings;
        use kcsdi_core::{
            commands::{Cal, Lo},
            device::SpecParams,
            model::Rbw,
        };
        let first = snapshot(&trace(&[1.0, 2.0, 3.0]));
        let AcquisitionSettings::Spec(base) = first.settings.clone() else {
            unreachable!()
        };
        let changed_settings = [
            SpecParams {
                cal: Cal::CalSys,
                ..base.clone()
            },
            SpecParams {
                lo: Lo::LowLo,
                ..base.clone()
            },
            SpecParams {
                rbw: Rbw::R1k,
                ..base.clone()
            },
            SpecParams {
                ref_level_dbm: -20,
                ..base.clone()
            },
        ];
        let mut changed: Vec<_> = changed_settings
            .into_iter()
            .map(|settings| CompletedSweep {
                settings: AcquisitionSettings::Spec(settings),
                ..snapshot(&trace(&[-1.0, -2.0, -3.0]))
            })
            .collect();
        changed.push(CompletedSweep {
            session_id: 2,
            ..snapshot(&trace(&[-1.0, -2.0, -3.0]))
        });
        for next in changed {
            let mut tools = AnalysisTools {
                hold: true,
                max_hold: true,
                min_hold: true,
                ..Default::default()
            };
            tools.observe(&first);
            tools.observe(&next);
            assert_eq!(tools.held_trace(), Some(next.data.clone()));
            assert_eq!(tools.maxima, Some(values(&next.data)));
            assert_eq!(tools.minima, Some(values(&next.data)));
        }
    }

    #[test]
    fn calibration_boundary_retains_definitions_and_never_combines_old_holds() {
        let before = snapshot(&trace(&[100.0, 200.0, 300.0]));
        let after = snapshot(&trace(&[-1.0, -2.0, -3.0]));
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&before);
        tools.add_marker(&before.data, 2e6);
        let config = tools.config();
        tools.invalidate_measurements();
        assert_eq!(tools.config(), config);
        assert!(tools.latest.is_none());
        assert!(tools.held_trace().is_none());
        assert!(tools.overlay_series(&[0]).is_empty());
        // A hold toggle against a retained complete frame must remain hidden
        // and cannot seed the first post-calibration envelope.
        tools.refresh_holds(&before.data);
        assert!(tools.overlay_series(&[0]).is_empty());
        tools.observe(&after);
        assert_eq!(tools.held_trace(), Some(after.data.clone()));
        assert_eq!(tools.maxima, Some(values(&after.data)));
        assert_eq!(tools.minima, Some(values(&after.data)));
        assert_eq!(tools.config(), config);
    }

    #[test]
    fn timestamps_do_not_reset_holds_and_shared_snapshots_keep_analysis_independent() {
        let first = snapshot(&trace(&[1.0, 2.0, 3.0]));
        let mut next = snapshot(&trace(&[4.0, 5.0, 6.0]));
        next.completed_at += std::time::Duration::from_secs(1);
        let mut frozen = AnalysisTools {
            hold: true,
            max_hold: true,
            ..Default::default()
        };
        let mut live = AnalysisTools::default();
        frozen.set_trace_id(1);
        live.set_trace_id(2);
        for tools in [&mut frozen, &mut live] {
            tools.observe(&first);
            tools.observe(&next);
        }
        assert_eq!(frozen.held_trace(), Some(first.data.clone()));
        assert_eq!(frozen.maxima, Some(values(&next.data)));
        assert!(live.held_trace().is_none());
        assert!(live.maxima.is_none());
        frozen.reset_holds();
        frozen.observe(&next);
        assert_eq!(frozen.held_trace(), Some(next.data));
        assert_eq!(first.data.points[0].values, vec![1.0]);
    }

    #[test]
    fn targets_search_and_read_the_same_rows_without_falling_back() {
        let first = trace(&[8.0, 1.0, 3.0]);
        let second = trace(&[2.0, 9.0, 0.0]);
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&snapshot(&first));
        tools.observe(&snapshot(&second));
        tools.add_marker(&second, 2e6);
        for (target, frequency, value) in [
            (MarkerTarget::Current, 2e6, 9.0),
            (MarkerTarget::Hold, 1e6, 8.0),
            (MarkerTarget::Maximum, 2e6, 9.0),
            (MarkerTarget::Minimum, 1e6, 2.0),
        ] {
            tools.target = target;
            tools.search(&second, Search::Maximum);
            assert_eq!(tools.markers[0].frequency_hz, frequency);
            assert_eq!(
                tools.target_sample(&second, frequency, 0),
                Some((frequency, value))
            );
        }
        tools.target = MarkerTarget::Hold;
        assert_eq!(tools.marker_trace(), Some(first.clone()));
        tools.hold = false;
        assert!(tools.target_sample(&second, 2e6, 0).is_none());
        assert!(tools.marker_trace().is_none());
        tools.search(&second, Search::Maximum);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        tools.hold = true;
        let mut changed_grid = second;
        changed_grid.points[1].freq_hz += 1.0;
        assert!(tools.target_sample(&changed_grid, 2e6, 0).is_none());
        for target in [MarkerTarget::Maximum, MarkerTarget::Minimum] {
            tools.target = target;
            assert!(tools.marker_trace().is_none());
        }
        tools.restore_config(&tools.config());
        assert!(tools.target_sample(&first, 2e6, 0).is_none());
        assert!(tools.marker_trace().is_none());
    }

    #[test]
    fn auto_peak_uses_complete_targets_and_manual_moves_disable_only_that_marker() {
        let first = trace(&[8.0, 1.0, 3.0]);
        let second = trace(&[2.0, 9.0, 0.0]);
        let mut tools = AnalysisTools {
            hold: true,
            ..Default::default()
        };
        tools.observe(&snapshot(&first));
        tools.add_marker(&first, 2e6);
        tools.markers[0].auto_peak = true;
        tools.add_marker(&first, 3e6);
        tools.observe(&snapshot(&second));
        assert_eq!(tools.markers[0].frequency_hz, 2e6);
        assert_eq!(tools.markers[1].frequency_hz, 3e6);
        assert!(!tools.markers[1].auto_peak);
        tools.target = MarkerTarget::Hold;
        tools.refresh_auto_peaks(&second);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        tools.markers[0].selected = true;
        tools.markers[1].selected = false;
        tools.move_selected(&second, 2.6e6);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        assert!(!tools.markers[0].auto_peak);
        tools.markers[0].auto_peak = true;
        tools.search(&second, Search::Left);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        assert!(!tools.markers[0].auto_peak);
        tools.markers[0].auto_peak = true;
        tools.search(&second, Search::Left);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        assert!(!tools.markers[0].auto_peak);
    }

    #[test]
    fn hold_toggles_do_not_create_an_acquisition_or_retarget_an_auto_marker() {
        let data = trace(&[9.0, 2.0, 3.0]);
        let complete = snapshot(&data);
        let mut tools = AnalysisTools::default();
        tools.observe(&complete);
        tools.add_marker(&data, 2e6);
        tools.markers[0].auto_peak = true;
        tools.hold = true;
        tools.max_hold = true;
        tools.refresh_holds(&data);
        assert_eq!(tools.markers[0].frequency_hz, 2e6);
        assert_eq!(
            tools.latest.as_ref().unwrap().completed_at,
            complete.completed_at
        );
        assert_eq!(tools.latest.as_ref().unwrap().data, data);
        assert_eq!(tools.held_trace(), Some(data));
    }

    #[test]
    fn selected_component_survives_receiver_grid_and_session_changes() {
        use crate::acquisition::AcquisitionSettings;
        use kcsdi_core::model::Rbw;
        let mut data = trace(&[50.0, 60.0, 70.0]);
        data.mode = StreamMode::S11;
        data.format = "z".into();
        for point in &mut data.points {
            point.values = vec![50.0, 30.0, -40.0];
        }
        let mut complete = snapshot(&data);
        let mut tools = AnalysisTools::default();
        tools.observe(&complete);
        tools.column = 1;
        let AcquisitionSettings::S11(params) = &mut complete.settings else {
            unreachable!()
        };
        params.rbw = Some(Rbw::R1k);
        tools.observe(&complete);
        assert_eq!(tools.column, 1);
        complete.data.points[1].freq_hz += 1.0;
        tools.observe(&complete);
        assert_eq!(tools.column, 1);
        complete.session_id += 1;
        tools.observe(&complete);
        assert_eq!(tools.column, 1);
        tools.restore_config(&tools.config());
        tools.observe(&complete);
        assert_eq!(tools.column, 1);
        tools.observe(&snapshot(&trace(&[1.0, 2.0, 3.0])));
        assert_eq!(tools.column, 0);
    }

    #[test]
    fn centers_and_side_searches_use_measured_frequency_and_finite_first_ties() {
        let mut data = trace(&[9.0, 4.0, f64::NAN, 9.0, f64::INFINITY]);
        data.points[1].freq_hz = data.points[0].freq_hz;
        let mut tools = AnalysisTools::default();
        tools.add_marker(&data, 3e6);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        tools.search(&data, Search::Maximum);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        tools.search(&data, Search::Right);
        assert_eq!(tools.markers[0].frequency_hz, 4e6);
        tools.search(&data, Search::Right);
        assert_eq!(tools.markers[0].frequency_hz, 4e6);
        tools.markers[0].auto_peak = true;
        tools.move_selected(&data, 2.1e6);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        assert!(!tools.markers[0].auto_peak);
        tools.move_selected(&data, f64::NAN);
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
    }

    #[test]
    fn marker_config_sanitizes_definitions_and_does_not_wrap_exhausted_ids() {
        let marker = |id, frequency_hz| Marker {
            id,
            frequency_hz,
            selected: true,
            reference: true,
            auto_peak: true,
        };
        let mut config = AnalysisConfig {
            markers: vec![
                marker(0, 1e6),
                marker(1, f64::NAN),
                marker(2, -1.0),
                marker(3, 0.0),
                marker(3, 2e6),
            ],
            next_id: 15,
            hold: true,
            column: usize::MAX,
            target: MarkerTarget::Hold,
            overlay_visibility: vec![
                OverlayVisibilityConfig {
                    format: "z".into(),
                    kind: 0,
                    column: 1,
                    visible: false,
                },
                OverlayVisibilityConfig {
                    format: "unknown".into(),
                    kind: 0,
                    column: 0,
                    visible: true,
                },
                OverlayVisibilityConfig {
                    format: "z".into(),
                    kind: 3,
                    column: 0,
                    visible: true,
                },
            ],
            ..Default::default()
        };
        config
            .markers
            .extend((4..20).map(|id| marker(id, f64::from(id) * 1e6)));
        let mut tools = AnalysisTools::default();
        tools.restore_config(&config);
        assert_eq!(tools.markers.len(), MAX_MARKERS);
        assert_eq!(tools.markers[0].frequency_hz, 0.0);
        assert_eq!(
            tools
                .markers
                .iter()
                .filter(|marker| marker.selected)
                .count(),
            1
        );
        assert_eq!(
            tools
                .markers
                .iter()
                .filter(|marker| marker.reference)
                .count(),
            1
        );
        assert_eq!(tools.column, 2);
        assert_eq!(tools.overlay_visibility.len(), 1);
        assert!(tools.latest.is_none() && tools.held.is_none());
        let restored = tools.config();
        let serialized = toml::to_string(&restored).unwrap();
        assert_eq!(
            toml::from_str::<AnalysisConfig>(&serialized).unwrap(),
            restored
        );
        tools.markers.clear();
        tools.add_marker(&trace(&[1.0, 2.0, 3.0]), 2e6);
        assert_eq!(tools.markers[0].id, 16);
        config.markers = vec![marker(u32::MAX, 1e6)];
        config.next_id = 0;
        tools.restore_config(&config);
        tools.markers.clear();
        tools.add_marker(&trace(&[1.0, 2.0, 3.0]), 2e6);
        assert!(tools.markers.is_empty());
        assert_eq!(tools.next_id, u32::MAX);
    }

    #[test]
    fn smith_uses_whole_current_or_held_impedance_and_clears_scalar_actions() {
        let mut data = trace(&[0.0, 0.5, 1.0]);
        data.mode = StreamMode::S11;
        data.format = "ri".into();
        for point in &mut data.points {
            point.values.push(0.0);
        }
        let mut tools = AnalysisTools {
            hold: true,
            ..Default::default()
        };
        tools.observe(&snapshot(&data));
        tools.add_marker(&data, 2e6);
        tools.markers[0].reference = true;
        tools.markers[0].auto_peak = true;
        tools.target = MarkerTarget::Maximum;
        tools.set_smith(true);
        assert_eq!(tools.target, MarkerTarget::Current);
        assert!(!tools.markers[0].reference && !tools.markers[0].auto_peak);
        assert_eq!(tools.smith_sample(&data, 1e6), Some((1e6, 50.0, 0.0)));
        assert_eq!(tools.smith_sample(&data, 2e6), Some((2e6, 150.0, 0.0)));
        assert!(tools.smith_sample(&data, 3e6).is_none());
        let first = data.clone();
        data.points[1].values = vec![0.0, -0.5];
        tools.observe(&snapshot(&data));
        assert_eq!(tools.smith_sample(&data, 2e6), Some((2e6, 30.0, -40.0)));
        tools.target = MarkerTarget::Hold;
        assert_eq!(tools.marker_trace(), Some(first));
        assert_eq!(tools.smith_sample(&data, 2e6), Some((2e6, 150.0, 0.0)));
        let mut config = tools.config();
        config.markers[0].reference = true;
        config.markers[0].auto_peak = true;
        config.target = MarkerTarget::Minimum;
        tools.restore_config(&config);
        assert_eq!(tools.target, MarkerTarget::Current);
        assert!(!tools.markers[0].reference && !tools.markers[0].auto_peak);
    }

    #[test]
    fn spectrum_delta_readout_identifies_reference_target_and_difference_units() {
        let data = trace(&[-30.0, -10.0, -20.0]);
        let mut tools = AnalysisTools::default();
        tools.add_marker(&data, 1e6);
        tools.markers[0].reference = true;
        tools.add_marker(&data, 2e6);
        for language in Language::ALL {
            let ctx = egui::Context::default();
            let output = ctx.run_ui(Default::default(), |ui| {
                tools.marker_table(ui, language, &data)
            });
            let text: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect();
            assert!(
                text.contains(&format!("M1 {}", language.text(Text::AnalysisReference)).as_str())
            );
            assert!(text.contains(&format!("M2 {}", language.text(Text::AnalysisDelta)).as_str()));
            assert!(
                text.contains(
                    &format!(
                        "{}: {}",
                        language.text(Text::AnalysisTarget),
                        language.text(Text::AnalysisCurrent)
                    )
                    .as_str()
                )
            );
            assert!(text.contains(&"dBm") && text.contains(&"dB"));
            assert!(text.contains(&"20") && text.contains(&"-30"));
            output.drop_without_applying_deltas();
        }
        assert_eq!(difference_unit(&data, 0, true), "dB");
        assert_eq!(difference_unit(&data, 0, false), "dBm");
    }

    fn controls_frame(
        ctx: &egui::Context,
        tools: &mut AnalysisTools,
        complete: &CompletedSweep,
        language: Language,
        frame: usize,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 1000.0),
                )),
                time: Some(frame as f64 / 60.0),
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::right("analysis_panel")
                    .exact_size(256.0)
                    .frame(egui::Frame::new().inner_margin(8))
                    .show(ui, |ui| {
                        let edge = ui.max_rect().right();
                        tools.controls_for_display(
                            ui,
                            language,
                            Some(complete),
                            tools.smith,
                            2e6,
                            true,
                        );
                        assert!(
                            ui.min_rect().right() <= edge + 0.1,
                            "analysis controls overflow a 240 px panel"
                        );
                    });
            },
        )
    }

    fn text_rect(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.galley.rect.translate(text.pos.to_vec2()))
            }
            _ => None,
        })
    }

    #[test]
    fn marker_frequency_grid_uses_the_chosen_completed_target_and_column() {
        let first = trace(&[10.0, f64::NAN, 30.0]);
        let second = trace(&[f64::NAN, 20.0, 40.0]);
        let mut tools = AnalysisTools {
            hold: true,
            max_hold: true,
            min_hold: true,
            ..Default::default()
        };
        tools.observe(&snapshot(&first));
        tools.observe(&snapshot(&second));
        for (target, expected) in [
            (MarkerTarget::Current, vec![2e6, 3e6]),
            (MarkerTarget::Hold, vec![1e6, 3e6]),
            (MarkerTarget::Maximum, vec![1e6, 2e6, 3e6]),
            (MarkerTarget::Minimum, vec![1e6, 2e6, 3e6]),
        ] {
            tools.target = target;
            assert_eq!(tools.marker_frequencies(&second), expected);
        }
        tools.target = MarkerTarget::Hold;
        let mut different = second.clone();
        different.points[0].freq_hz += 1.0;
        assert!(tools.marker_frequencies(&different).is_empty());
        tools.hold = false;
        assert!(tools.marker_frequencies(&second).is_empty());
        tools.target = MarkerTarget::Current;
        tools.column = 1;
        assert!(tools.marker_frequencies(&second).is_empty());
    }

    #[test]
    fn marker_frequency_grid_preserves_reported_order_and_duplicates() {
        let mut data = trace(&[1.0, 2.0, 3.0, 4.0]);
        for (point, frequency) in data.points.iter_mut().zip([3e6, 2e6, 2e6, f64::INFINITY]) {
            point.freq_hz = frequency;
        }
        assert_eq!(
            AnalysisTools::default().marker_frequencies(&data),
            [3e6, 2e6, 2e6]
        );
    }

    #[test]
    fn bilingual_controls_fit_a_narrow_panel_and_tables_identify_trace_and_component() {
        for language in Language::ALL {
            for (format, magnitudes, smith) in [
                ("z", [1e200, -1e200, 2e200], false),
                ("delay", [-5e-12, 3e-12, 1e-12], false),
                ("ri", [0.0, 0.25, 0.5], true),
            ] {
                let mut data = trace(&magnitudes);
                data.mode = if format == "delay" {
                    StreamMode::S21
                } else {
                    StreamMode::S11
                };
                data.format = format.into();
                for (index, point) in data.points.iter_mut().enumerate() {
                    point.freq_hz = 6_000_000_000.0 + index as f64 * 1e6;
                    if format == "z" {
                        point.values = vec![point.values[0].abs(), point.values[0], 1e200];
                    }
                    if format == "ri" {
                        point.values.push(-0.25);
                    }
                }
                let complete = snapshot(&data);
                let mut tools = AnalysisTools {
                    hold: true,
                    trace_id: 42,
                    ..Default::default()
                };
                tools.set_smith(smith);
                tools.observe(&complete);
                tools.target = MarkerTarget::Hold;
                tools.add_marker(&data, data.points[0].freq_hz);
                tools.markers[0].reference = !smith;
                tools.add_marker(&data, data.points[1].freq_hz);
                if format == "z" {
                    tools.column = 1;
                }
                let ctx = egui::Context::default();
                for frame in 0..3 {
                    let output =
                        controls_frame(&ctx, &mut tools, &complete, language, frame, vec![]);
                    if frame == 2 {
                        assert!(
                            text_rect(
                                &output,
                                &format!("T42 {}", language.text(Text::AnalysisMarkers))
                            )
                            .is_some()
                        );
                        assert!(
                            text_rect(
                                &output,
                                &format!(
                                    "{}: {}",
                                    language.text(Text::AnalysisTarget),
                                    language.text(Text::AnalysisHold)
                                )
                            )
                            .is_some()
                        );
                        assert!(text_rect(&output, language.text(Text::AnalysisCenter)).is_some());
                        if smith {
                            assert!(text_rect(&output, "R (ohm)").is_some());
                            assert!(text_rect(&output, "X (ohm)").is_some());
                            for key in [
                                Text::AnalysisAutoPeak,
                                Text::AnalysisMaximum,
                                Text::AnalysisDeltaReference,
                            ] {
                                assert!(text_rect(&output, language.text(key)).is_none());
                            }
                        } else {
                            assert!(
                                text_rect(
                                    &output,
                                    &format!(
                                        "{}: {}",
                                        language.text(Text::AnalysisColumn),
                                        column_label(format, tools.column, language)
                                    )
                                )
                                .is_some()
                            );
                            assert!(
                                text_rect(
                                    &output,
                                    &format!("M2 {}", language.text(Text::AnalysisDelta))
                                )
                                .is_some()
                            );
                        }
                    }
                    output.drop_without_applying_deltas();
                }
            }
        }
    }

    #[test]
    fn stopped_hold_reset_recaptures_and_retargets_auto_peak_without_a_new_sweep() {
        let first = trace(&[9.0, 2.0, 3.0]);
        let second = trace(&[1.0, 2.0, 8.0]);
        let complete = snapshot(&second);
        let mut tools = AnalysisTools {
            hold: true,
            target: MarkerTarget::Hold,
            ..Default::default()
        };
        tools.observe(&snapshot(&first));
        tools.add_marker(&first, 2e6);
        tools.markers[0].auto_peak = true;
        tools.observe(&complete);
        assert_eq!(tools.markers[0].frequency_hz, 1e6);
        let ctx = egui::Context::default();
        let mut button = None;
        for frame in 0..3 {
            let output = controls_frame(
                &ctx,
                &mut tools,
                &complete,
                Language::English,
                frame,
                vec![],
            );
            button = text_rect(&output, Language::English.text(Text::AnalysisReset));
            output.drop_without_applying_deltas();
        }
        let position = button.unwrap().center();
        for (frame, pressed) in [(3, true), (4, false)] {
            controls_frame(
                &ctx,
                &mut tools,
                &complete,
                Language::English,
                frame,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .drop_without_applying_deltas();
        }
        assert_eq!(tools.held_trace(), Some(second));
        assert_eq!(tools.markers[0].frequency_hz, 3e6);
        assert!(tools.markers[0].auto_peak);
        assert_eq!(
            tools.latest.as_ref().unwrap().completed_at,
            complete.completed_at
        );
    }

    #[test]
    fn disabling_holds_without_display_data_discards_buffers_before_reenabling() {
        let first = snapshot(&trace(&[-1.0, 8.0, 3.0]));
        let current = snapshot(&trace(&[2.0, 3.0, 9.0]));
        for (kind, caption) in [
            Text::AnalysisHold,
            Text::AnalysisMaxHold,
            Text::AnalysisMinHold,
        ]
        .into_iter()
        .enumerate()
        {
            let mut tools = AnalysisTools {
                hold: true,
                max_hold: true,
                min_hold: true,
                ..Default::default()
            };
            tools.observe(&first);
            tools.observe(&current);
            let buffer = |tools: &AnalysisTools| match kind {
                0 => tools.held.clone(),
                1 => tools.maxima.clone(),
                _ => tools.minima.clone(),
            };
            assert!(buffer(&tools).is_some());
            let ctx = egui::Context::default();
            let mut frame = 0;
            let mut render =
                |tools: &mut AnalysisTools, complete: Option<&CompletedSweep>, events| {
                    frame += 1;
                    ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1280.0, 1000.0),
                            )),
                            time: Some(frame as f64 / 60.0),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::Panel::right("analysis_no_data")
                                .exact_size(256.0)
                                .show(ui, |ui| {
                                    tools.controls_for_display(
                                        ui,
                                        Language::English,
                                        complete,
                                        false,
                                        2e6,
                                        true,
                                    );
                                });
                        },
                    )
                };
            let mut click = |tools: &mut AnalysisTools, complete: Option<&CompletedSweep>| {
                let mut button = None;
                for _ in 0..3 {
                    let output = render(tools, complete, vec![]);
                    // HOLD appears in both the section heading and the button.
                    button = output
                        .shapes
                        .iter()
                        .rev()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::Text(text)
                                if text.galley.text() == Language::English.text(caption) =>
                            {
                                Some(text.galley.rect.translate(text.pos.to_vec2()))
                            }
                            _ => None,
                        });
                    output.drop_without_applying_deltas();
                }
                let position = button.unwrap().center();
                for pressed in [true, false] {
                    render(
                        tools,
                        complete,
                        vec![
                            egui::Event::PointerMoved(position),
                            egui::Event::PointerButton {
                                pos: position,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    )
                    .drop_without_applying_deltas();
                }
            };
            click(&mut tools, None);
            assert!(
                buffer(&tools).is_none(),
                "disabled buffer {kind} survived absent display data"
            );
            click(&mut tools, None);
            assert!(
                buffer(&tools).is_none(),
                "enabling buffer {kind} captured absent display data"
            );
            click(&mut tools, None);
            click(&mut tools, Some(&current));
            assert_eq!(buffer(&tools), Some(values(&current.data)));
            assert_eq!(tools.latest.as_ref().unwrap().data, current.data);
            assert_eq!(
                tools.latest.as_ref().unwrap().completed_at,
                current.completed_at
            );
        }
    }
}
