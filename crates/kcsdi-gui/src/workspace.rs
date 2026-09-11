// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Independent trace definitions sharing one frequency range and device worker.

use std::sync::Arc;

use egui::Color32;
use kcsdi_core::commands::{Cal, Lo};
use kcsdi_core::device::{PointSettings, S11Params, S21Params, SpecParams};
use kcsdi_core::model::{FreqRange, Rbw};
use kcsdi_core::validation::frequency_hz;

use crate::acquisition::{AcquisitionSettings, CompletedSweep, SweepPlan, TraceId};
use crate::analysis_tools::AnalysisTools;
use crate::i18n::{Language, Text};
use crate::preview::PreviewEnvelope;
use crate::state::{AppMode, DEVICE_MODEL, S11Display, S21Display};
use crate::widgets::plot::{PlotView, YScale};
use crate::widgets::smith::SmithView;

pub use crate::acquisition::MAX_TRACES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceDisplay {
    #[default]
    Spec,
    S11(S11Display),
    S21(S21Display),
}

impl TraceDisplay {
    pub fn mode(self) -> AppMode {
        match self {
            Self::Spec => AppMode::Spec,
            Self::S11(_) => AppMode::S11,
            Self::S21(_) => AppMode::S21,
        }
    }

    pub fn label(self, language: Language) -> String {
        match self {
            Self::Spec => language.text(Text::Spectrum).to_owned(),
            Self::S11(display) => format!("S11 {}", display.label(language)),
            Self::S21(display) => format!("S21 {}", display.label(language)),
        }
    }

    pub fn is_smith(self) -> bool {
        self == Self::S11(S11Display::Smith)
    }

    pub fn logarithmic_y(self) -> Option<YScale> {
        match self {
            Self::S11(
                S11Display::Impedance
                | S11Display::Magnitude
                | S11Display::Resistance
                | S11Display::Reactance,
            ) => Some(YScale::LogImpedance),
            Self::S11(S11Display::Vswr) => Some(YScale::LogVswr),
            _ => None,
        }
    }

    pub fn default_y(self) -> (f64, f64) {
        match self {
            Self::Spec => (-100.0, 0.0),
            Self::S11(display) => display.default_y(),
            Self::S21(display) => display.default_y(),
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Spec => "dBm",
            Self::S11(display) => display.y_label(),
            Self::S21(display) => display.y_label(),
        }
    }

    pub fn columns(self) -> &'static [usize] {
        match self {
            Self::S11(S11Display::Phase) | Self::S21(S21Display::Phase) => &[1],
            Self::S11(S11Display::Resistance) => &[1],
            Self::S11(S11Display::Reactance) => &[2],
            Self::S11(S11Display::Impedance) => &[0, 1, 2],
            Self::S11(S11Display::Smith) => &[],
            _ => &[0],
        }
    }

    pub fn accepts(self, data: &kcsdi_core::data::SweepData) -> bool {
        use kcsdi_core::protocol::StreamMode;
        match self {
            Self::Spec => data.mode == StreamMode::Spec && data.format.is_empty(),
            Self::S11(display) => {
                data.mode == StreamMode::S11 && data.format == display.wire_format().as_str()
            }
            Self::S21(display) => {
                data.mode == StreamMode::S21 && data.format == display.wire_format().as_str()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceSettings {
    pub display: TraceDisplay,
    pub cal: Cal,
    pub rbw: Rbw,
    pub lo: Lo,
    pub ref_level_dbm: i32,
    pub visible: bool,
    pub color: Color32,
    pub line_width: f32,
    pub impedance_visible: [bool; 3],
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self {
            display: TraceDisplay::Spec,
            cal: Cal::CalOff,
            rbw: Rbw::R10k,
            lo: Lo::HighLo,
            ref_level_dbm: -10,
            visible: true,
            color: crate::theme::TRACE_COLORS[0],
            line_width: 1.0,
            impedance_visible: [true; 3],
        }
    }
}

impl TraceSettings {
    pub fn acquisition_for(
        &self,
        range: &SweepRange,
        list: Option<&[u64]>,
    ) -> kcsdi_core::Result<AcquisitionSettings> {
        if let Some(frequencies_hz) = list {
            let settings = match self.display {
                TraceDisplay::Spec => PointSettings::Spec {
                    cal: self.cal,
                    lo: self.lo,
                    rbw: self.rbw,
                    ref_level_dbm: self.ref_level_dbm,
                },
                TraceDisplay::S11(display) => PointSettings::S11 {
                    cal: self.cal,
                    format: display.wire_format(),
                    rbw: Some(self.rbw),
                },
                TraceDisplay::S21(display) => PointSettings::S21 {
                    cal: self.cal,
                    format: display.wire_format(),
                    lo: self.lo,
                    rbw: Some(self.rbw),
                },
            };
            let settings = AcquisitionSettings::List {
                settings,
                frequencies_hz: frequencies_hz.to_vec(),
            };
            settings.validate()?;
            return Ok(settings);
        }
        self.acquisition(range)
    }

    pub fn acquisition(&self, range: &SweepRange) -> kcsdi_core::Result<AcquisitionSettings> {
        let start_hz = frequency_hz(range.start_hz, "start")?;
        let stop_hz = frequency_hz(range.stop_hz, "stop")?;
        let settings = match self.display {
            TraceDisplay::Spec => AcquisitionSettings::Spec(SpecParams {
                cal: self.cal,
                lo: self.lo,
                points: range.points,
                start_hz,
                stop_hz,
                rbw: self.rbw,
                ref_level_dbm: self.ref_level_dbm,
            }),
            TraceDisplay::S11(display) => AcquisitionSettings::S11(S11Params {
                cal: self.cal,
                format: display.wire_format(),
                points: range.points,
                start_hz,
                stop_hz,
                rbw: Some(self.rbw),
            }),
            TraceDisplay::S21(display) => AcquisitionSettings::S21(S21Params {
                cal: self.cal,
                format: display.wire_format(),
                lo: self.lo,
                points: range.points,
                start_hz,
                stop_hz,
                rbw: Some(self.rbw),
            }),
        };
        settings.validate()?;
        Ok(settings)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepRange {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub center_hz: f64,
    pub span_hz: f64,
    pub points: u32,
}

impl Default for SweepRange {
    fn default() -> Self {
        Self::new(100e6, 500e6, 201)
    }
}

impl SweepRange {
    pub fn new(start_hz: f64, stop_hz: f64, points: u32) -> Self {
        Self {
            start_hz,
            stop_hz,
            center_hz: (start_hz + stop_hz) / 2.0,
            span_hz: stop_hz - start_hz,
            points,
        }
    }

    pub fn start_stop_changed(&mut self) {
        self.center_hz = (self.start_hz + self.stop_hz) / 2.0;
        self.span_hz = self.stop_hz - self.start_hz;
    }

    pub fn center_span_changed(&mut self) {
        self.start_hz = self.center_hz - self.span_hz / 2.0;
        self.stop_hz = self.center_hz + self.span_hz / 2.0;
    }
}

pub struct TraceState {
    pub id: TraceId,
    pub settings: TraceSettings,
    pub view: PlotView,
    pub needs_fit: bool,
    pub view_locked: bool,
    pub analysis: AnalysisTools,
    pub completed: Option<Arc<CompletedSweep>>,
    pub preview: Option<Arc<PreviewEnvelope>>,
    pub last_completed_cycle: Option<u64>,
}

impl TraceState {
    pub fn new(id: TraceId, settings: TraceSettings, range: &SweepRange) -> Self {
        let (low, high) = settings.display.default_y();
        let mut analysis = AnalysisTools::default();
        analysis.set_trace_id(id.0);
        analysis.set_smith(settings.display.is_smith());
        let columns = settings.display.columns();
        analysis.set_column((columns.len() == 1).then(|| columns[0]));
        Self {
            id,
            settings,
            view: PlotView::new(range.start_hz, range.stop_hz, low, high),
            needs_fit: true,
            view_locked: false,
            analysis,
            completed: None,
            preview: None,
            last_completed_cycle: None,
        }
    }

    pub fn update_settings(&mut self, settings: TraceSettings) {
        if self.settings.display != settings.display {
            self.view.y_scale = YScale::Linear;
            let (low, high) = settings.display.default_y();
            self.view.y_min = low;
            self.view.y_max = high;
            self.needs_fit = true;
            self.view_locked = false;
        }
        let columns = settings.display.columns();
        self.analysis
            .set_column((columns.len() == 1).then(|| columns[0]));
        self.analysis.set_smith(settings.display.is_smith());
        self.settings = settings;
    }

    pub fn completed_for_display(&self) -> Option<&CompletedSweep> {
        self.completed
            .as_deref()
            .filter(|snapshot| self.settings.display.accepts(&snapshot.data))
    }

    pub fn display_data(&self) -> Option<&kcsdi_core::data::SweepData> {
        self.preview
            .as_deref()
            .filter(|preview| {
                !preview.data.points.is_empty() && self.settings.display.accepts(&preview.data)
            })
            .map(|preview| &preview.data)
            .or_else(|| self.completed_for_display().map(|snapshot| &snapshot.data))
    }

    pub fn overlays_compatible(&self) -> bool {
        let Some(completed) = self.completed_for_display() else {
            return false;
        };
        self.preview
            .as_ref()
            .filter(|preview| !preview.data.points.is_empty())
            .is_none_or(|preview| {
                preview.session_id == completed.session_id
                    && preview.group.settings == completed.settings
            })
    }
}

pub struct TraceEditor {
    pub id: Option<TraceId>,
    pub settings: TraceSettings,
}

pub struct Workspace {
    pub range: SweepRange,
    pub list_mode: bool,
    pub frequencies_hz: Vec<u64>,
    pub frequency_editor: crate::frequency_editor::FrequencyEditor,
    pub run: crate::run_settings::RunSettings,
    pub run_editor: crate::run_settings::RunSettingsEditor,
    pub traces: Vec<TraceState>,
    pub selected: Option<TraceId>,
    pub x_view: PlotView,
    pub log_x: bool,
    pub smith: SmithView,
    pub next_id: u64,
    pub editor: Option<TraceEditor>,
}

impl Default for Workspace {
    fn default() -> Self {
        let range = SweepRange::default();
        let mut workspace = Self::empty(range);
        workspace
            .add_trace(TraceSettings::default())
            .expect("default trace");
        workspace
    }
}

impl Workspace {
    pub fn open_add_editor(&mut self) {
        let color = crate::theme::TRACE_COLORS
            .iter()
            .copied()
            .find(|color| {
                self.traces
                    .iter()
                    .all(|trace| trace.settings.color != *color)
            })
            .unwrap_or(
                crate::theme::TRACE_COLORS[self.traces.len() % crate::theme::TRACE_COLORS.len()],
            );
        self.editor = Some(TraceEditor {
            id: None,
            settings: TraceSettings {
                color,
                ..Default::default()
            },
        });
    }

    pub fn empty(range: SweepRange) -> Self {
        Self {
            range,
            list_mode: false,
            frequencies_hz: Vec::new(),
            frequency_editor: Default::default(),
            run: Default::default(),
            run_editor: Default::default(),
            traces: Vec::new(),
            selected: None,
            x_view: PlotView::new(range.start_hz, range.stop_hz, 0.0, 1.0),
            log_x: false,
            smith: SmithView::default(),
            next_id: 1,
            editor: None,
        }
    }

    pub fn add_trace(&mut self, settings: TraceSettings) -> kcsdi_core::Result<TraceId> {
        if self.traces.len() >= MAX_TRACES {
            return Err(kcsdi_core::Error::InvalidParameter(
                "at most ten traces are supported".into(),
            ));
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| kcsdi_core::Error::InvalidParameter("trace IDs exhausted".into()))?;
        let id = TraceId(self.next_id);
        self.next_id = next;
        self.traces.push(TraceState::new(id, settings, &self.range));
        self.selected = Some(id);
        Ok(id)
    }

    pub fn remove_trace(&mut self, id: TraceId) {
        self.traces.retain(|trace| trace.id != id);
        if self.selected == Some(id) {
            self.selected = self.traces.last().map(|trace| trace.id);
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.id == Some(id))
        {
            self.editor = None;
        }
    }

    pub fn selected(&self) -> Option<&TraceState> {
        self.selected
            .and_then(|id| self.traces.iter().find(|trace| trace.id == id))
    }

    pub fn selected_mut(&mut self) -> Option<&mut TraceState> {
        self.selected
            .and_then(|id| self.traces.iter_mut().find(|trace| trace.id == id))
    }

    pub fn plan(&self) -> kcsdi_core::Result<SweepPlan> {
        let requests: kcsdi_core::Result<Vec<_>> = self
            .traces
            .iter()
            .filter(|trace| trace.settings.visible)
            .map(|trace| {
                trace
                    .settings
                    .acquisition_for(&self.range, self.requested_list())
                    .map(|settings| (trace.id, settings))
            })
            .collect();
        let mut plan = SweepPlan::from_requests(requests?)?;
        plan.run = self.run.clone();
        plan.validate()?;
        Ok(plan)
    }

    pub fn requested_list(&self) -> Option<&[u64]> {
        self.list_mode.then_some(&self.frequencies_hz)
    }

    pub fn frequency_bounds(&self) -> (f64, f64) {
        if self.list_mode
            && let (Some(&first), Some(&last)) =
                (self.frequencies_hz.first(), self.frequencies_hz.last())
        {
            if first == last {
                let pad = (first as f64 * 0.01).max(1.0);
                return ((first as f64 - pad).max(0.0), last as f64 + pad);
            }
            return (first as f64, last as f64);
        }
        (self.range.start_hz, self.range.stop_hz)
    }

    pub fn reset_frequency_view(&mut self) {
        (self.x_view.x_min, self.x_view.x_max) = self.frequency_bounds();
        self.ensure_log_x_view();
    }

    pub fn visible_range(&self) -> FreqRange {
        let caps = DEVICE_MODEL.capabilities();
        self.traces
            .iter()
            .filter(|trace| trace.settings.visible)
            .fold(caps.spec.range, |range, trace| {
                let next = match trace.settings.display.mode() {
                    AppMode::Spec => caps.spec.range,
                    AppMode::S11 => caps.s11.range,
                    AppMode::S21 => caps.s21.range,
                };
                FreqRange::new(
                    range.min_hz.max(next.min_hz),
                    range.max_hz.min(next.max_hz),
                    range.min_span_hz.max(next.min_span_hz),
                )
            })
    }

    /// Explicit C=M acquisition edit. Keep the span unless a mode boundary
    /// requires a smaller symmetric sweep around the marker (section 8.3).
    pub fn center_on_marker(&mut self, frequency: f64) -> kcsdi_core::Result<()> {
        if self.list_mode {
            return Err(kcsdi_core::Error::InvalidParameter(
                "marker centering requires a frequency range".into(),
            ));
        }
        let invalid = || {
            kcsdi_core::Error::InvalidParameter(
                "marker center cannot form a valid sweep for the visible traces".into(),
            )
        };
        let limits = self.visible_range();
        let center = frequency_hz(frequency, "marker center")? as f64;
        let start = frequency_hz(self.range.start_hz, "start")?;
        let stop = frequency_hz(self.range.stop_hz, "stop")?;
        if !limits.contains_sweep(start, stop) {
            return Err(invalid());
        }
        let half_span = (center - limits.min_hz as f64).min(limits.max_hz as f64 - center);
        if half_span < limits.min_span_hz as f64 / 2.0 {
            return Err(invalid());
        }
        let span = (stop - start) as f64;
        let span = span.min(half_span * 2.0);
        let start = (center - span / 2.0).ceil() as u64;
        let stop = (center + span / 2.0).ceil() as u64;
        if !limits.contains_sweep(start, stop) {
            return Err(invalid());
        }
        self.range = SweepRange::new(start as f64, stop as f64, self.range.points);
        self.x_view.x_min = self.range.start_hz;
        self.x_view.x_max = self.range.stop_hz;
        self.ensure_log_x_view();
        Ok(())
    }

    pub fn clear_previews(&mut self) {
        for trace in &mut self.traces {
            trace.preview = None;
            trace.last_completed_cycle = None;
        }
    }

    /// Keep log conversion independent of whichever partial group is visible.
    pub fn ensure_log_x_view(&mut self) {
        if !self.log_x
            || (self.x_view.x_min.is_finite()
                && self.x_view.x_min > 0.0
                && self.x_view.x_max.is_finite()
                && self.x_view.x_max > self.x_view.x_min)
        {
            return;
        }
        let (start, planned_stop) = self.frequency_bounds();
        let stop = if self.x_view.x_max.is_finite() && self.x_view.x_max > 0.0 {
            self.x_view.x_max
        } else if planned_stop.is_finite() && planned_stop > 0.0 {
            planned_stop
        } else {
            10.0
        };
        let measured_start = self
            .traces
            .iter()
            .filter(|trace| trace.settings.visible && !trace.settings.display.is_smith())
            .filter_map(TraceState::completed_for_display)
            .flat_map(|snapshot| snapshot.data.points.iter())
            .map(|point| point.freq_hz)
            .filter(|frequency| frequency.is_finite() && *frequency > 0.0 && *frequency < stop)
            .min_by(f64::total_cmp);
        let step = (planned_stop - start) / f64::from(self.range.points.saturating_sub(1).max(1));
        let grid_start = if self.list_mode {
            self.frequencies_hz
                .iter()
                .map(|&hz| hz as f64)
                .find(|&hz| hz > 0.0 && hz < stop)
                .unwrap_or(stop / 10.0)
        } else if start > 0.0 {
            start
        } else {
            step
        };
        self.x_view.x_min = measured_start
            .filter(|_| !self.list_mode)
            .unwrap_or_else(|| {
                if grid_start.is_finite() && grid_start > 0.0 && grid_start < stop {
                    grid_start
                } else {
                    stop / 10.0
                }
            });
        self.x_view.x_max = stop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_center_keeps_span_and_y_scale_and_shrinks_only_at_limits() {
        let mut workspace = Workspace {
            range: SweepRange::new(100e6, 500e6, 201),
            ..Default::default()
        };
        let view = workspace.traces[0].view;
        workspace.center_on_marker(600e6).unwrap();
        assert_eq!(workspace.range, SweepRange::new(400e6, 800e6, 201));
        assert_eq!(workspace.x_view.x_min, 400e6);
        assert_eq!(workspace.x_view.x_max, 800e6);
        assert_eq!(workspace.traces[0].view, view);

        workspace.center_on_marker(100e6).unwrap();
        assert_eq!(workspace.range, SweepRange::new(0.0, 200e6, 201));
        workspace.center_on_marker(6_999_999_500.0).unwrap();
        assert_eq!(workspace.range, SweepRange::new(6_999_999_000.0, 7e9, 201));
        workspace.plan().unwrap();
    }

    #[test]
    fn marker_center_intersects_visible_mode_limits_and_rejects_invalid_edits() {
        let mut workspace = Workspace::default();
        workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::ReturnLoss),
                ..Default::default()
            })
            .unwrap();
        workspace.center_on_marker(5500.0).unwrap();
        assert_eq!(workspace.range, SweepRange::new(5000.0, 6000.0, 201));
        workspace.plan().unwrap();
        for center in [0.0, 5499.0, 7e9, -1.0, f64::NAN, f64::INFINITY] {
            let old = workspace.range;
            assert!(workspace.center_on_marker(center).is_err());
            assert_eq!(workspace.range, old);
        }
        workspace.range.stop_hz = 4000.0;
        assert!(workspace.center_on_marker(1e6).is_err());
        assert_eq!(workspace.range.stop_hz, 4000.0);
    }

    #[test]
    fn marker_center_preserves_odd_integer_span_and_valid_log_view() {
        let mut workspace = Workspace {
            range: SweepRange::new(10_000.0, 11_001.0, 3),
            ..Default::default()
        };
        workspace.center_on_marker(20_000.0).unwrap();
        assert_eq!(workspace.range, SweepRange::new(19_500.0, 20_501.0, 3));
        workspace.log_x = true;
        workspace.center_on_marker(500.0).unwrap();
        assert_eq!(workspace.range, SweepRange::new(0.0, 1000.0, 3));
        assert!(workspace.x_view.x_min > 0.0);
        assert_eq!(workspace.x_view.x_max, 1000.0);
    }

    #[test]
    fn add_editors_choose_distinct_colors_without_changing_existing_styles() {
        let mut workspace = Workspace::default();
        let first = workspace.traces[0].settings.color;
        workspace.open_add_editor();
        let draft = workspace.editor.take().unwrap();
        assert_ne!(draft.settings.color, first);
        workspace.add_trace(draft.settings).unwrap();
        workspace.open_add_editor();
        let third = workspace.editor.take().unwrap().settings.color;
        assert!(
            workspace
                .traces
                .iter()
                .all(|trace| trace.settings.color != third)
        );
        assert_eq!(workspace.traces[0].settings.color, first);
    }

    #[test]
    fn trace_ids_survive_deletion_and_the_limit_is_enforced_in_state() {
        let mut workspace = Workspace::empty(SweepRange::default());
        let ids: Vec<_> = (0..MAX_TRACES)
            .map(|_| workspace.add_trace(TraceSettings::default()).unwrap())
            .collect();
        assert!(workspace.add_trace(TraceSettings::default()).is_err());
        workspace.remove_trace(ids[3]);
        let added = workspace.add_trace(TraceSettings::default()).unwrap();
        assert!(added.0 > ids.last().unwrap().0);
        workspace.remove_trace(added);
        assert_eq!(workspace.selected, ids.last().copied());
        for id in ids {
            workspace.remove_trace(id);
        }
        assert!(workspace.traces.is_empty());
        assert!(workspace.selected.is_none());
        assert!(workspace.plan().is_err());
    }

    #[test]
    fn only_visible_traces_constrain_frequency_and_participate_in_the_plan() {
        let mut workspace = Workspace {
            range: SweepRange::new(0.0, 1e6, 3),
            ..Default::default()
        };
        let id = workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Impedance),
                visible: false,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(workspace.visible_range().min_hz, 0);
        assert_eq!(workspace.plan().unwrap().groups.len(), 1);
        workspace.selected = Some(id);
        workspace.selected_mut().unwrap().settings.visible = true;
        assert_eq!(workspace.visible_range().min_hz, 5000);
        assert!(workspace.plan().is_err());
        for trace in &mut workspace.traces {
            trace.settings.visible = false;
        }
        assert!(workspace.plan().is_err());
    }

    #[test]
    fn transmission_groups_include_mode_format_calibration_lo_and_bandwidth() {
        let mut workspace = Workspace::empty(SweepRange::new(1e6, 2e6, 3));
        let base = TraceSettings {
            display: TraceDisplay::S21(S21Display::Phase),
            ..Default::default()
        };
        let first = workspace.add_trace(base.clone()).unwrap();
        let second = workspace.add_trace(base.clone()).unwrap();
        for settings in [
            TraceSettings {
                display: TraceDisplay::S11(S11Display::Phase),
                ..base.clone()
            },
            TraceSettings {
                display: TraceDisplay::S21(S21Display::Loss),
                ..base.clone()
            },
            TraceSettings {
                display: TraceDisplay::S21(S21Display::Delay),
                ..base.clone()
            },
            TraceSettings {
                cal: Cal::CalSys,
                ..base.clone()
            },
            TraceSettings {
                lo: Lo::LowLo,
                ..base.clone()
            },
            TraceSettings {
                rbw: Rbw::R3k,
                ..base.clone()
            },
        ] {
            workspace.add_trace(settings).unwrap();
        }
        let plan = workspace.plan().unwrap();
        assert_eq!(plan.groups.len(), 7);
        assert_eq!(plan.groups[0].members, [first, second]);
        assert!(
            matches!(&plan.groups[0].settings, AcquisitionSettings::S21(params)
            if params.format == kcsdi_core::commands::Format::Ma && params.rbw == Some(Rbw::R10k))
        );
        workspace.range = SweepRange::new(0.0, 1e6, 3);
        assert!(workspace.plan().is_err());
        workspace.traces[2].settings.visible = false;
        assert_eq!(workspace.visible_range().min_hz, 0);
        assert!(workspace.plan().is_ok());
    }

    #[test]
    fn impedance_projections_and_smith_share_wire_data_but_not_receiver_changes() {
        let mut workspace = Workspace::empty(SweepRange::new(1e6, 2e6, 3));
        for display in [
            S11Display::Impedance,
            S11Display::Magnitude,
            S11Display::Resistance,
            S11Display::Reactance,
            S11Display::Smith,
        ] {
            workspace
                .add_trace(TraceSettings {
                    display: TraceDisplay::S11(display),
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(workspace.plan().unwrap().groups.len(), 1);
        workspace.traces[1].settings.rbw = Rbw::R3k;
        workspace.traces[2].settings.cal = Cal::CalSys;
        assert_eq!(workspace.plan().unwrap().groups.len(), 3);
        assert!(matches!(
            workspace.plan().unwrap().groups[0].settings,
            AcquisitionSettings::S11(S11Params { rbw: Some(_), .. })
        ));
    }
}
