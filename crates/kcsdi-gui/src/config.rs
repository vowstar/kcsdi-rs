// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! User configuration, stored in the platform-standard config directory.
//!
//! Locations (via the `directories` crate, `ProjectDirs::from("io.github",
//! "vowstar", "kcsdi")`):
//!
//! - Linux: `~/.config/kcsdi/config.toml` (XDG_CONFIG_HOME)
//! - macOS: `~/Library/Application Support/io.github.vowstar.kcsdi/config.toml`
//! - Windows: `%APPDATA%\vowstar\kcsdi\config\config.toml`
//!
//! `KCSDI_CONFIG_PATH` overrides the file path (used by tests). Only GUI
//! settings live here; never secrets. Large or machine-specific data does
//! not belong in this file.

use std::path::PathBuf;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use kcsdi_core::commands::{Cal, Lo};
use kcsdi_core::model::Rbw;

use crate::acquisition::{MAX_TRACES, TraceId};
use crate::analysis_tools::AnalysisConfig;
use crate::desktop::{DesktopConfig, DeviceProfile};
use crate::i18n::LanguagePreference;
use crate::state::{AppMode, AppState, S11Display, S21Display};
use crate::widgets::{plot::PlotView, smith::SmithView};
use crate::workspace::{SweepMode, SweepRange, TraceDisplay, TraceSettings, TraceState, Workspace};

/// Environment variable that overrides the config file path.
pub const ENV_CONFIG_PATH: &str = "KCSDI_CONFIG_PATH";

/// Current config schema version.
pub const CONFIG_VERSION: u32 = 10;

fn legacy_config_version() -> u32 {
    1
}

/// Persisted user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "legacy_config_version")]
    pub version: u32,
    /// Missing or unknown language preferences follow the system locale.
    pub language: LanguagePreference,
    /// Read-only migration fields from the fixed-mode workspace.
    #[serde(skip_serializing)]
    pub mode: String,
    pub connection: kcsdi_core::connection::ConnectionTarget,
    #[serde(skip_serializing)]
    pub spec: Spec,
    #[serde(skip_serializing)]
    pub s11: S11,
    /// Original fixed-mode settings kept for recovery, never used for acquisition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_sweeps: Option<LegacySweeps>,
    pub workspace: WorkspaceConfig,
    pub desktop: DesktopConfig,
    pub function: crate::source_panel::InstrumentFunction,
    pub sources: crate::source_panel::SourceConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacySweeps {
    pub mode: String,
    pub spec: Spec,
    pub s11: S11,
}

/// Workspace definitions only. Measurements are never written to user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: u32,
    #[serde(skip_serializing)]
    pub list_mode: bool,
    #[serde(default)]
    pub sweep_mode: Option<SweepMode>,
    pub segments: Vec<kcsdi_core::segments::Segment>,
    pub frequencies_hz: Vec<u64>,
    pub run: crate::run_settings::RunSettings,
    pub log_x: bool,
    pub selected: Option<TraceId>,
    pub next_id: u64,
    pub traces: Vec<TraceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_view: Option<XViewConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smith_view: Option<SmithViewConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    pub id: TraceId,
    pub display: String,
    pub cal: String,
    pub rbw: String,
    pub lo: String,
    pub ref_level_dbm: i32,
    pub visible: bool,
    pub color: [u8; 4],
    pub line_width: f32,
    pub impedance_visible: [bool; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_view: Option<YViewConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<AnalysisConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct XViewConfig {
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct YViewConfig {
    pub min: f64,
    pub max: f64,
    pub divisions: usize,
    pub locked: bool,
    pub log_y: bool,
}

impl Default for YViewConfig {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            divisions: 8,
            locked: false,
            log_y: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmithViewConfig {
    pub zoom: f64,
    pub dx: f64,
    pub dy: f64,
}

impl Default for SmithViewConfig {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            dx: 0.0,
            dy: 0.0,
        }
    }
}

fn valid_span(min: f64, max: f64) -> bool {
    min.is_finite() && max.is_finite() && max > min && (max - min).is_finite()
}

impl YViewConfig {
    fn valid(self) -> bool {
        valid_span(self.min, self.max)
            && (self.min + self.max).is_finite()
            && (self.max - self.min) / self.divisions.clamp(2, 30) as f64 > 0.0
    }
}

impl XViewConfig {
    fn for_workspace(workspace: &Workspace) -> Self {
        let (start, stop) = workspace.frequency_bounds();
        let mut view = Self::for_range(
            &SweepRange::new(start, stop, workspace.range.points),
            workspace.log_x,
        );
        if workspace.log_x
            && start <= 0.0
            && let Some(positive) = workspace.positive_grid_start().filter(|&hz| hz < stop)
        {
            view.min = positive;
        }
        view
    }

    fn valid(self, log_x: bool) -> bool {
        valid_span(self.min, self.max)
            && (!log_x || (self.min > 0.0 && self.max.log10() > self.min.log10()))
    }

    fn for_range(range: &SweepRange, log_x: bool) -> Self {
        let mut view = Self {
            min: range.start_hz,
            max: range.stop_hz,
        };
        if log_x && valid_span(view.min, view.max) && view.min <= 0.0 && view.max > 0.0 {
            let step = (view.max - view.min) / f64::from(range.points.saturating_sub(1).max(1));
            view.min = if step > 0.0 && step < view.max {
                step
            } else {
                view.max / 10.0
            };
        }
        if view.valid(log_x) {
            view
        } else {
            let range = SweepRange::default();
            Self {
                min: range.start_hz,
                max: range.stop_hz,
            }
        }
    }
}

impl SmithViewConfig {
    fn restore(self) -> SmithView {
        // Leave room for the widget's largest zoom and large pixel viewports.
        // Pan has no UI bound, but converting the mapped coordinates must not overflow f32.
        let pan_limit = f64::from(f32::MAX) / (200.0 * 1_000_000.0);
        let pan = |offset: f64| {
            if offset.is_finite() && offset.abs() <= pan_limit {
                offset
            } else {
                0.0
            }
        };
        SmithView {
            zoom: if self.zoom.is_finite() {
                self.zoom.clamp(0.2, 200.0)
            } else {
                1.0
            },
            dx: pan(self.dx),
            dy: pan(self.dy),
        }
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self::from_workspace(&Workspace::default())
    }
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self::from_settings(TraceId(1), &TraceSettings::default())
    }
}

impl TraceConfig {
    fn from_settings(id: TraceId, settings: &TraceSettings) -> Self {
        Self {
            id,
            display: match settings.display {
                TraceDisplay::Spec => "spec",
                TraceDisplay::S11(display) => display_as_str(display),
                TraceDisplay::S21(display) => match display {
                    S21Display::Phase => "s21_phase",
                    S21Display::Loss => "s21_loss",
                    S21Display::Delay => "s21_delay",
                },
            }
            .to_owned(),
            cal: settings.cal.as_str().to_owned(),
            rbw: settings.rbw.as_str().to_owned(),
            lo: settings.lo.as_str().to_owned(),
            ref_level_dbm: settings.ref_level_dbm,
            visible: settings.visible,
            color: settings.color.to_array(),
            line_width: settings.line_width,
            impedance_visible: settings.impedance_visible,
            y_view: None,
            analysis: None,
        }
    }

    fn from_trace(trace: &TraceState) -> Self {
        let mut config = Self::from_settings(trace.id, &trace.settings);
        let valid_y = YViewConfig {
            min: trace.view.y_min,
            max: trace.view.y_max,
            divisions: trace.view.y_divisions,
            locked: trace.view_locked,
            log_y: trace.view.y_scale.floor().is_some(),
        }
        .valid();
        let (min, max) = if valid_y {
            (trace.view.y_min, trace.view.y_max)
        } else {
            trace.settings.display.default_y()
        };
        config.y_view = Some(YViewConfig {
            min,
            max,
            divisions: trace.view.y_divisions.clamp(2, 30),
            locked: valid_y && trace.view_locked,
            log_y: trace.view.y_scale.floor().is_some(),
        });
        config.analysis = Some(trace.analysis.config());
        config
    }

    fn restore(&self, range: &SweepRange, x_view: XViewConfig) -> TraceState {
        let mut trace = TraceState::new(self.id, self.settings(), range);
        trace.view.x_min = x_view.min;
        trace.view.x_max = x_view.max;
        if let Some(view) = self.y_view {
            if view.log_y
                && let Some(scale) = trace.settings.display.logarithmic_y()
            {
                trace.view.y_scale = scale;
            }
            trace.view.y_divisions = view.divisions.clamp(2, 30);
            if view.valid()
                && trace
                    .view
                    .y_scale
                    .floor()
                    .is_none_or(|floor| view.min >= floor && view.max.log10() > view.min.log10())
            {
                trace.view.y_min = view.min;
                trace.view.y_max = view.max;
                trace.view_locked = view.locked;
                trace.needs_fit = !view.locked;
            }
            trace.view.ensure_y_view();
        }
        if let Some(analysis) = &self.analysis {
            trace.analysis.restore_config(analysis);
            let display = trace.settings.display;
            trace.analysis.set_column(
                display
                    .columns()
                    .first()
                    .copied()
                    .filter(|_| display.columns().len() == 1),
            );
            trace.analysis.set_smith(display.is_smith());
        }
        trace
    }

    fn settings(&self) -> TraceSettings {
        let display = match self.display.as_str() {
            "s21_phase" => TraceDisplay::S21(S21Display::Phase),
            "s21_loss" => TraceDisplay::S21(S21Display::Loss),
            "s21_delay" => TraceDisplay::S21(S21Display::Delay),
            "phase" | "return_loss" | "vswr" | "smith" | "impedance" | "magnitude"
            | "resistance" | "reactance" => TraceDisplay::S11(parse_display(&self.display)),
            _ => TraceDisplay::Spec,
        };
        TraceSettings {
            display,
            cal: self.cal.parse().unwrap_or(Cal::CalOff),
            rbw: self.rbw.parse().unwrap_or(Rbw::R10k),
            lo: self.lo.parse().unwrap_or(Lo::HighLo),
            ref_level_dbm: self.ref_level_dbm,
            visible: self.visible,
            color: egui::Color32::from_rgba_premultiplied(
                self.color[0],
                self.color[1],
                self.color[2],
                self.color[3],
            ),
            line_width: if self.line_width.is_finite() {
                self.line_width.clamp(0.5, 5.0)
            } else {
                1.0
            },
            impedance_visible: self.impedance_visible,
        }
    }
}

impl WorkspaceConfig {
    fn from_workspace(workspace: &Workspace) -> Self {
        let x_view = XViewConfig {
            min: workspace.x_view.x_min,
            max: workspace.x_view.x_max,
        };
        let x_view = if x_view.valid(workspace.log_x) {
            x_view
        } else {
            XViewConfig::for_workspace(workspace)
        };
        let smith = SmithViewConfig {
            zoom: workspace.smith.zoom,
            dx: workspace.smith.dx,
            dy: workspace.smith.dy,
        }
        .restore();
        Self {
            start_hz: workspace.range.start_hz,
            stop_hz: workspace.range.stop_hz,
            points: workspace.range.points,
            list_mode: false,
            sweep_mode: Some(workspace.sweep_mode),
            segments: workspace.segments.clone(),
            frequencies_hz: workspace.frequencies_hz.clone(),
            run: workspace.run.clone(),
            log_x: workspace.log_x,
            selected: workspace.selected,
            next_id: workspace.next_id,
            x_view: Some(x_view),
            smith_view: Some(SmithViewConfig {
                zoom: smith.zoom,
                dx: smith.dx,
                dy: smith.dy,
            }),
            traces: workspace
                .traces
                .iter()
                .map(TraceConfig::from_trace)
                .collect(),
        }
    }

    fn restore(&self) -> Workspace {
        let mut workspace =
            Workspace::empty(SweepRange::new(self.start_hz, self.stop_hz, self.points));
        workspace.log_x = self.log_x;
        workspace.sweep_mode = self.sweep_mode.unwrap_or(if self.list_mode {
            SweepMode::List
        } else {
            SweepMode::Range
        });
        if self.segments.len() <= kcsdi_core::segments::MAX_SEGMENTS {
            workspace.segments = self.segments.clone();
        } else {
            warn!("segment definitions exceed the application limit, edit the plan before running");
        }
        workspace.frequencies_hz = self.frequencies_hz.clone();
        workspace.run = self.run.clone();
        let x_view = self
            .x_view
            .filter(|view| view.valid(self.log_x))
            .unwrap_or_else(|| XViewConfig::for_workspace(&workspace));
        workspace.x_view = PlotView::new(x_view.min, x_view.max, 0.0, 1.0);
        workspace.smith = self.smith_view.unwrap_or_default().restore();
        for trace in &self.traces {
            if workspace.traces.len() == MAX_TRACES {
                warn!("ignoring workspace definitions beyond the ten-trace limit");
                break;
            }
            if trace.id.0 == 0
                || trace.id.0 == u64::MAX
                || workspace.traces.iter().any(|old| old.id == trace.id)
            {
                warn!(
                    "ignoring invalid or repeated workspace trace ID {}",
                    trace.id.0
                );
                continue;
            }
            workspace.next_id = workspace.next_id.max(trace.id.0 + 1);
            workspace
                .traces
                .push(trace.restore(&workspace.range, x_view));
        }
        workspace.next_id = workspace.next_id.max(self.next_id);
        workspace.selected = self
            .selected
            .filter(|id| workspace.traces.iter().any(|trace| trace.id == *id))
            .or_else(|| workspace.traces.last().map(|trace| trace.id));
        workspace
    }
}

/// Last-used SPEC sweep parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spec {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: u32,
    /// Wire literal of the RBW (`Rbw::as_str`).
    pub rbw: String,
    pub ref_level_dbm: i32,
    pub log_x: bool,
}

/// Last-used S11 sweep parameters and display settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct S11 {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: u32,
    /// Wire literal of the calibration mode (`Cal::as_str`).
    pub cal: String,
    /// Display format key ("phase", "return_loss", "vswr", "smith",
    /// "impedance").
    pub display: String,
    /// Logarithmic frequency axis for cartesian displays.
    pub log_x: bool,
    /// Wire literal of the RBW (`Rbw::as_str`), or None for device default.
    pub rbw: Option<String>,
}

/// String mapping helpers for the persisted mode and display keys.
fn mode_as_str(mode: AppMode) -> &'static str {
    match mode {
        AppMode::Spec => "spec",
        AppMode::S11 => "s11",
        AppMode::S21 => "s21",
    }
}

fn parse_mode(s: &str) -> AppMode {
    match s {
        "s11" => AppMode::S11,
        _ => AppMode::Spec,
    }
}

fn display_as_str(display: S11Display) -> &'static str {
    match display {
        S11Display::Phase => "phase",
        S11Display::ReturnLoss => "return_loss",
        S11Display::Vswr => "vswr",
        S11Display::Smith => "smith",
        S11Display::Impedance => "impedance",
        S11Display::Magnitude => "magnitude",
        S11Display::Resistance => "resistance",
        S11Display::Reactance => "reactance",
    }
}

fn parse_display(s: &str) -> S11Display {
    match s {
        "phase" => S11Display::Phase,
        "vswr" => S11Display::Vswr,
        "smith" => S11Display::Smith,
        "impedance" => S11Display::Impedance,
        "magnitude" => S11Display::Magnitude,
        "resistance" => S11Display::Resistance,
        "reactance" => S11Display::Reactance,
        _ => S11Display::ReturnLoss,
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            language: LanguagePreference::default(),
            mode: mode_as_str(AppMode::default()).to_string(),
            connection: Default::default(),
            spec: Spec::default(),
            s11: S11::default(),
            legacy_sweeps: None,
            workspace: WorkspaceConfig::default(),
            desktop: DesktopConfig::default(),
            function: Default::default(),
            sources: Default::default(),
        }
    }
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            start_hz: 100e6,
            stop_hz: 500e6,
            points: 201,
            rbw: "10k".to_string(),
            ref_level_dbm: -10,
            log_x: false,
        }
    }
}

impl Default for S11 {
    fn default() -> Self {
        Self {
            start_hz: 1e6,
            stop_hz: 1000e6,
            points: 201,
            cal: Cal::CalOff.as_str().to_string(),
            display: display_as_str(S11Display::default()).to_string(),
            log_x: false,
            rbw: None,
        }
    }
}

impl AppConfig {
    /// Snapshot the persisted fields of the current UI state.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            version: CONFIG_VERSION,
            language: state.language_preference,
            legacy_sweeps: state.legacy_sweeps.clone(),
            connection: state.target.clone(),
            workspace: WorkspaceConfig::from_workspace(&state.workspace),
            desktop: state.desktop.settings.clone(),
            function: state.function,
            sources: state.source.config.clone(),
            ..Self::default()
        }
    }

    /// Apply loaded settings to freshly initialized UI state. Unknown
    /// strings fall back to defaults instead of failing.
    pub fn apply_to(&self, state: &mut AppState) {
        state.desktop.settings = self.desktop.clone();
        // Import the legacy connection once. A version-2 empty profile list
        // represents the user's choice and must stay empty after deletion.
        if self.version < 2
            && state.desktop.settings.profiles.is_empty()
            && let kcsdi_core::connection::ConnectionTarget::Tcp { host, port } = &self.connection
        {
            let host = host.trim();
            if !host.is_empty() {
                state.desktop.settings.profiles.push(DeviceProfile {
                    name: host.to_owned(),
                    target: kcsdi_core::connection::ConnectionTarget::Tcp {
                        host: host.to_owned(),
                        port: *port,
                    },
                });
            }
        }
        state.set_language_preference(self.language);
        state.target = self.connection.clone();
        state.legacy_sweeps = if self.version < 3 {
            Some(LegacySweeps {
                mode: self.mode.clone(),
                spec: self.spec.clone(),
                s11: self.s11.clone(),
            })
        } else {
            self.legacy_sweeps.clone()
        };
        state.workspace = if self.version < 3 {
            self.legacy_workspace()
        } else {
            self.workspace.restore()
        };
        state.sweep = crate::state::SweepState::Idle;
        state.active_plan = None;
        state.run_progress = None;
        state.last_recording = None;
        state.calibration = Default::default();
        state.function = self.function;
        state.source = crate::source_panel::SourceUi {
            config: self.sources.clone(),
            ..Default::default()
        };
    }

    fn legacy_workspace(&self) -> Workspace {
        let selected = parse_mode(&self.mode);
        let (range, log_x) = match selected {
            AppMode::Spec | AppMode::S21 => (
                SweepRange::new(self.spec.start_hz, self.spec.stop_hz, self.spec.points),
                self.spec.log_x,
            ),
            AppMode::S11 => (
                SweepRange::new(self.s11.start_hz, self.s11.stop_hz, self.s11.points),
                self.s11.log_x,
            ),
        };
        let mut workspace = Workspace::empty(range);
        workspace.log_x = log_x;
        let spec = workspace
            .add_trace(TraceSettings {
                rbw: self.spec.rbw.parse().unwrap_or(Rbw::R10k),
                ref_level_dbm: self.spec.ref_level_dbm,
                visible: selected == AppMode::Spec,
                ..Default::default()
            })
            .expect("legacy SPEC definition");
        let s11 = workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(parse_display(&self.s11.display)),
                cal: self.s11.cal.parse().unwrap_or(Cal::CalOff),
                rbw: self
                    .s11
                    .rbw
                    .as_deref()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(Rbw::R10k),
                visible: selected == AppMode::S11,
                color: crate::theme::TRACE_COLORS[1],
                ..Default::default()
            })
            .expect("legacy S11 definition");
        workspace.selected = Some(match selected {
            AppMode::Spec | AppMode::S21 => spec,
            AppMode::S11 => s11,
        });
        WorkspaceConfig::from_workspace(&workspace).restore()
    }
}

/// Resolve the config file path: env override first, then the
/// platform-standard config directory.
pub fn config_path() -> Option<PathBuf> {
    config_path_with_override(std::env::var(ENV_CONFIG_PATH).ok().as_deref())
}

fn config_path_with_override(path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = path
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    directories::ProjectDirs::from("io.github", "vowstar", "kcsdi")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

/// Load the config, falling back to defaults on any error.
pub fn load() -> AppConfig {
    let Some(path) = config_path() else {
        warn!("no config directory available, using defaults");
        return AppConfig::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => {
                info!("loaded config from {}", path.display());
                cfg
            }
            Err(e) => {
                warn!("invalid config {}: {e}, using defaults", path.display());
                AppConfig::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AppConfig::default(),
        Err(e) => {
            warn!("cannot read config {}: {e}, using defaults", path.display());
            AppConfig::default()
        }
    }
}

/// Save the config atomically (write temp file, then rename).
pub fn save(cfg: &AppConfig) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_tools::{MarkerTarget, OverlayVisibilityConfig};
    use crate::widgets::plot::Marker;

    #[test]
    fn version_three_keeps_workspace_identity_and_uses_its_range_for_missing_views() {
        let config: AppConfig = toml::from_str(
            r#"
version = 3
[workspace]
start_hz = 2000000.0
stop_hz = 9000000.0
points = 101
selected = 55
next_id = 88
[[workspace.traces]]
id = 55
display = "s21_delay"
visible = false
"#,
        )
        .unwrap();
        assert!(config.workspace.x_view.is_none());
        assert!(config.workspace.traces[0].y_view.is_none());
        assert!(config.workspace.traces[0].analysis.is_none());
        let mut state = AppState::default();
        config.apply_to(&mut state);
        assert_eq!(state.workspace.traces.len(), 1);
        assert_eq!(state.workspace.selected, Some(TraceId(55)));
        assert_eq!(state.workspace.next_id, 88);
        assert!(!state.workspace.traces[0].settings.visible);
        assert_eq!(
            (state.workspace.x_view.x_min, state.workspace.x_view.x_max),
            (2e6, 9e6)
        );
        let trace = &state.workspace.traces[0];
        assert_eq!(
            (trace.view.y_min, trace.view.y_max),
            S21Display::Delay.default_y()
        );
        assert!(!trace.view_locked);
        assert!(trace.needs_fit);
        assert_eq!(
            state.connection,
            crate::state::ConnectionState::Disconnected
        );
        assert_eq!(state.sweep, crate::state::SweepState::Idle);
        assert!(state.legacy_sweeps.is_none());
        assert!(state.desktop.settings.profiles.is_empty());
        let saved = AppConfig::from_state(&state);
        assert_eq!(saved.version, CONFIG_VERSION);
        assert!(saved.workspace.x_view.is_some());
        assert!(saved.workspace.traces[0].analysis.is_some());
    }

    #[test]
    fn analysis_and_views_round_trip_without_acquired_rows_or_running_state() {
        use crate::acquisition::CompletedSweep;
        use kcsdi_core::data::{SweepData, SweepPoint};
        use kcsdi_core::protocol::StreamMode;
        use std::sync::Arc;
        use std::time::SystemTime;

        let mut state = AppState::default();
        state.workspace.range = SweepRange::new(1e6, 2e6, 3);
        state.workspace.x_view = PlotView::new(1.1e6, 1.9e6, 0.0, 1.0);
        state.workspace.log_x = true;
        state.workspace.smith = SmithView {
            zoom: 2.5,
            dx: -0.12,
            dy: 0.27,
        };
        let trace = &mut state.workspace.traces[0];
        trace.update_settings(TraceSettings {
            display: TraceDisplay::S21(S21Display::Delay),
            ..Default::default()
        });
        trace.view.y_min = -7.123456e-12;
        trace.view.y_max = 8.234567e-12;
        trace.view.y_divisions = 6;
        trace.view_locked = true;
        trace.analysis.restore_config(&AnalysisConfig {
            markers: vec![
                Marker {
                    id: 7,
                    frequency_hz: 1e6,
                    selected: false,
                    reference: true,
                    auto_peak: false,
                },
                Marker {
                    id: 9,
                    frequency_hz: 2e6,
                    selected: true,
                    reference: false,
                    auto_peak: false,
                },
            ],
            next_id: 15,
            hold: true,
            max_hold: true,
            min_hold: true,
            target: MarkerTarget::Hold,
            overlay_visibility: vec![OverlayVisibilityConfig {
                format: "delay".into(),
                kind: 1,
                column: 0,
                visible: false,
            }],
            ..Default::default()
        });
        let snapshot = CompletedSweep {
            segments: None,
            settings: trace.settings.acquisition(&state.workspace.range).unwrap(),
            session_id: 812,
            completed_at: SystemTime::UNIX_EPOCH,
            data: SweepData {
                mode: StreamMode::S21,
                format: "delay".into(),
                points: (0..3)
                    .map(|index| SweepPoint {
                        freq_hz: 1e6 + f64::from(index) * 0.5e6,
                        values: vec![f64::from(index) * 1e-12],
                    })
                    .collect(),
            },
        };
        trace.analysis.observe(&snapshot);
        assert!(trace.analysis.held_trace().is_some());
        trace.completed = Some(Arc::new(snapshot.clone()));
        trace.last_completed_cycle = Some(91);
        let expected_analysis = trace.analysis.config();
        let expected_y = trace.view;
        state.sweep = crate::state::SweepState::Running;
        state.active_plan = state.workspace.plan().ok();
        state.session_id = 812;
        state.request_id = 51;
        let before = AppConfig::from_state(&state);
        let encoded = toml::to_string_pretty(&before).unwrap();
        for runtime_field in [
            "session_id",
            "request_id",
            "completed",
            "values",
            "held =",
            "maxima",
            "minima",
            "active_plan",
            "running",
            "preview",
        ] {
            assert!(
                !encoded.contains(runtime_field),
                "unexpected runtime field: {runtime_field}"
            );
        }
        let loaded: AppConfig = toml::from_str(&encoded).unwrap();
        let mut restored = AppState::default();
        loaded.apply_to(&mut restored);
        assert_eq!(AppConfig::from_state(&restored), before);
        assert_eq!(restored.workspace.x_view, state.workspace.x_view);
        assert_eq!(restored.workspace.smith, state.workspace.smith);
        assert_eq!(
            restored.connection,
            crate::state::ConnectionState::Disconnected
        );
        assert_eq!(restored.sweep, crate::state::SweepState::Idle);
        assert!(restored.active_plan.is_none());
        assert_eq!((restored.session_id, restored.request_id), (0, 0));
        let trace = &mut restored.workspace.traces[0];
        assert_eq!(
            (trace.view.y_min, trace.view.y_max, trace.view.y_divisions),
            (expected_y.y_min, expected_y.y_max, expected_y.y_divisions)
        );
        assert!(trace.view_locked);
        assert!(!trace.needs_fit);
        assert!(trace.completed.is_none());
        assert!(trace.preview.is_none());
        assert!(trace.last_completed_cycle.is_none());
        assert_eq!(trace.analysis.config(), expected_analysis);
        assert!(trace.analysis.held_trace().is_none());
        assert!(trace.analysis.marker_trace().is_none());
        assert!(trace.analysis.overlay_series(&[0]).is_empty());
        let mut fresh = snapshot;
        fresh.session_id = 1;
        fresh.data.points[0].values[0] = 9.876543e-12;
        trace.analysis.observe(&fresh);
        assert_eq!(
            trace.analysis.held_trace().unwrap().points[0].values[0],
            9.876543e-12
        );
        assert_eq!(
            (trace.view.y_min, trace.view.y_max),
            (expected_y.y_min, expected_y.y_max)
        );
    }

    #[test]
    fn invalid_views_fall_back_without_rewriting_invalid_acquisition_fields() {
        for (min, max) in [
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
            (3.0, 3.0),
            (2.0, 1.0),
            (-f64::MAX, f64::MAX),
        ] {
            let config = WorkspaceConfig {
                start_hz: -10.0,
                stop_hz: -20.0,
                x_view: Some(XViewConfig { min, max }),
                traces: vec![TraceConfig {
                    display: "s21_delay".into(),
                    y_view: Some(YViewConfig {
                        min,
                        max,
                        divisions: 0,
                        locked: true,
                        log_y: false,
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let restored = config.restore();
            assert_eq!(
                (restored.range.start_hz, restored.range.stop_hz),
                (-10.0, -20.0)
            );
            assert!(restored.plan().is_err());
            assert!(valid_span(restored.x_view.x_min, restored.x_view.x_max));
            let trace = &restored.traces[0];
            assert_eq!(
                (trace.view.y_min, trace.view.y_max),
                S21Display::Delay.default_y()
            );
            assert_eq!(trace.view.y_divisions, 2);
            assert!(!trace.view_locked);
            assert!(trace.needs_fit);
        }
    }

    #[test]
    fn tiny_delay_views_and_unlocked_views_keep_their_fit_policy() {
        for locked in [false, true] {
            let view = YViewConfig {
                min: -3e-18,
                max: 5e-18,
                divisions: usize::MAX,
                locked,
                log_y: false,
            };
            let config = WorkspaceConfig {
                traces: vec![TraceConfig {
                    display: "s21_delay".into(),
                    y_view: Some(view),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let restored = config.restore();
            let trace = &restored.traces[0];
            assert_eq!((trace.view.y_min, trace.view.y_max), (view.min, view.max));
            assert_eq!(trace.view.y_divisions, 30);
            assert_eq!(trace.view_locked, locked);
            assert_eq!(trace.needs_fit, !locked);
        }
    }

    #[test]
    fn log_y_views_round_trip_and_reject_incompatible_units_and_bounds() {
        use crate::widgets::plot::YScale;
        for display in S11Display::ALL {
            let mut state = AppState::default();
            let trace = state.workspace.selected_mut().unwrap();
            trace.update_settings(TraceSettings {
                display: TraceDisplay::S11(display),
                ..Default::default()
            });
            if let Some(scale) = trace.settings.display.logarithmic_y() {
                trace.view.y_scale = scale;
                trace.view.y_min = scale.floor().unwrap();
                trace.view.y_max = 1e3;
                trace.view_locked = true;
            }
            let view = trace.view;
            let serialized = toml::to_string(&AppConfig::from_state(&state)).unwrap();
            let config: AppConfig = toml::from_str(&serialized).unwrap();
            let mut restored = AppState::default();
            config.apply_to(&mut restored);
            assert_eq!(restored.workspace.selected_mut().unwrap().view, view);
        }
        for (display, min, max, expected) in [
            ("impedance", -1.0, 1.0, YScale::LogImpedance),
            ("vswr", 1e-3, 100.0, YScale::LogVswr),
            ("s21_delay", -1e-9, 1e-9, YScale::Linear),
            ("spec", -100.0, 0.0, YScale::Linear),
        ] {
            let config = WorkspaceConfig {
                traces: vec![TraceConfig {
                    display: display.into(),
                    y_view: Some(YViewConfig {
                        min,
                        max,
                        locked: true,
                        log_y: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let restored = config.restore();
            let trace = &restored.traces[0];
            assert_eq!(trace.view.y_scale, expected);
            if let Some(floor) = expected.floor() {
                assert!(trace.view.y_min >= floor && trace.view.y_max > trace.view.y_min);
                assert!(!trace.view_locked && trace.needs_fit);
            } else {
                assert_eq!((trace.view.y_min, trace.view.y_max), (min, max));
            }
        }
    }

    #[test]
    fn log_view_recovery_keeps_the_configured_full_stop_and_accepts_linear_pan() {
        let config = WorkspaceConfig {
            start_hz: 0.0,
            stop_hz: 1e6,
            points: 201,
            log_x: true,
            x_view: Some(XViewConfig {
                min: -1e6,
                max: 0.0,
            }),
            ..Default::default()
        };
        let restored = config.restore();
        assert_eq!((restored.x_view.x_min, restored.x_view.x_max), (5e3, 1e6));
        assert_eq!(restored.range.start_hz, 0.0);
        let linear = WorkspaceConfig {
            log_x: false,
            ..config
        }
        .restore();
        assert_eq!((linear.x_view.x_min, linear.x_view.x_max), (-1e6, 0.0));
        assert!(
            !XViewConfig {
                min: 1e300,
                max: f64::from_bits(1e300_f64.to_bits() + 1)
            }
            .valid(true)
        );
    }

    #[test]
    fn smith_view_normalizes_nonfinite_and_overflowing_coordinates() {
        assert_eq!(
            SmithViewConfig {
                zoom: f64::NAN,
                dx: f64::INFINITY,
                dy: 1e300
            }
            .restore(),
            SmithView::default()
        );
        assert_eq!(
            SmithViewConfig {
                zoom: 0.01,
                dx: -0.7,
                dy: 0.3
            }
            .restore(),
            SmithView {
                zoom: 0.2,
                dx: -0.7,
                dy: 0.3
            }
        );
        assert_eq!(
            SmithViewConfig {
                zoom: 1000.0,
                ..Default::default()
            }
            .restore()
            .zoom,
            200.0
        );
    }

    #[test]
    fn restored_analysis_cannot_override_projection_columns_or_smith_restrictions() {
        let analysis = AnalysisConfig {
            markers: vec![Marker {
                id: 8,
                frequency_hz: 1e6,
                selected: true,
                reference: true,
                auto_peak: true,
            }],
            column: 2,
            target: MarkerTarget::Maximum,
            ..Default::default()
        };
        let config = WorkspaceConfig {
            traces: vec![
                TraceConfig {
                    id: TraceId(1),
                    display: "s21_phase".into(),
                    analysis: Some(analysis.clone()),
                    ..Default::default()
                },
                TraceConfig {
                    id: TraceId(2),
                    display: "smith".into(),
                    analysis: Some(analysis),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let restored = config.restore();
        assert_eq!(restored.traces[0].analysis.config().column, 1);
        let smith = restored.traces[1].analysis.config();
        assert_eq!(smith.target, MarkerTarget::Current);
        assert!(!smith.markers[0].reference);
        assert!(!smith.markers[0].auto_peak);
    }

    #[test]
    fn transmission_definitions_round_trip_without_reusing_s11_keys() {
        let mut state = AppState::default();
        for display in S21Display::ALL {
            state
                .workspace
                .add_trace(TraceSettings {
                    display: TraceDisplay::S21(display),
                    cal: Cal::CalUser,
                    lo: Lo::LowLo,
                    rbw: Rbw::R3k,
                    ..Default::default()
                })
                .unwrap();
        }
        state.send(crate::state::WorkerCommand::RunWorkspace(
            state.workspace.plan().unwrap(),
        ));
        let before = AppConfig::from_state(&state);
        let encoded = toml::to_string(&before).unwrap();
        for key in ["s21_phase", "s21_loss", "s21_delay"] {
            assert!(encoded.contains(key));
        }
        let loaded: AppConfig = toml::from_str(&encoded).unwrap();
        let mut restored = AppState::default();
        loaded.apply_to(&mut restored);
        assert_eq!(restored.sweep, crate::state::SweepState::Idle);
        assert!(restored.active_plan.is_none());
        assert_eq!(restored.workspace.selected, state.workspace.selected);
        assert_eq!(
            restored.workspace.plan().unwrap(),
            state.workspace.plan().unwrap()
        );
        assert_eq!(AppConfig::from_state(&restored), before);
    }
    use crate::theme::ThemeMode;

    #[test]
    fn default_roundtrip() {
        let cfg = AppConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn legacy_tcp_and_tagged_serial_profiles_share_one_config() {
        use kcsdi_core::connection::ConnectionTarget;
        let old = r#"
version = 7
language = "zh-CN"
[connection]
host = "instrument.local"
port = 4321
[[desktop.profiles]]
name = "TCP bench"
host = "instrument.local"
port = 4321
[[desktop.profiles]]
name = "USB bench"
kind = "serial"
path = "/dev/serial/by-id/unavailable"
"#;
        let config: AppConfig = toml::from_str(old).unwrap();
        let mut state = AppState::default();
        config.apply_to(&mut state);
        assert_eq!(state.target.to_string(), "instrument.local:4321");
        assert_eq!(state.desktop.settings.profiles.len(), 2);
        let serial = ConnectionTarget::Serial {
            path: "/dev/serial/by-id/unavailable".into(),
        };
        assert_eq!(state.desktop.settings.profiles[1].target, serial);
        state.target = serial.clone();
        let encoded = toml::to_string(&AppConfig::from_state(&state)).unwrap();
        assert!(encoded.contains("kind = \"serial\""));
        assert!(encoded.contains("kind = \"tcp\""));
        assert!(!encoded.contains("lookup"));
        let restored: AppConfig = toml::from_str(&encoded).unwrap();
        let mut restarted = AppState::default();
        restored.apply_to(&mut restarted);
        assert_eq!(restarted.target, serial);
        assert_eq!(restarted.desktop.settings, state.desktop.settings);
        assert_eq!(
            restarted.connection,
            crate::state::ConnectionState::Disconnected
        );
        assert!(!restarted.desktop.lookup.is_pending());
        assert!(!restarted.any_running());
        assert!(!restarted.source.busy());
        assert!(!restarted.calibration.busy());
    }

    #[test]
    fn version_nine_range_and_list_modes_migrate_without_changing_their_grids() {
        for list in [false, true] {
            let text = format!(
                "version = 9\n[workspace]\nlist_mode = {list}\nfrequencies_hz = [5000, 10000, 1000000]\nstart_hz = 1000000.0\nstop_hz = 3000000.0\npoints = 201\n"
            );
            let config: AppConfig = toml::from_str(&text).unwrap();
            assert!(config.workspace.sweep_mode.is_none());
            let mut state = AppState::default();
            config.apply_to(&mut state);
            assert_eq!(
                state.workspace.sweep_mode,
                if list {
                    SweepMode::List
                } else {
                    SweepMode::Range
                }
            );
            assert_eq!(state.workspace.frequencies_hz, [5000, 10000, 1000000]);
            assert_eq!(state.workspace.range, SweepRange::new(1e6, 3e6, 201));
            assert!(!state.any_running());
            let encoded = toml::to_string(&AppConfig::from_state(&state)).unwrap();
            assert!(!encoded.contains("list_mode"));
            assert!(encoded.contains("sweep_mode"));
        }
    }

    #[test]
    fn segmented_config_keeps_inactive_grids_views_and_invalid_plans_without_running() {
        let snapshot = crate::segmented::tests::example_snapshot();
        let crate::acquisition::AcquisitionSettings::Segments(plan) = &snapshot.settings else {
            unreachable!()
        };
        let mut state = AppState::default();
        state.workspace.sweep_mode = SweepMode::Segments;
        state.workspace.segments = plan.segments().iter().map(|row| row.definition()).collect();
        state.workspace.selected_mut().unwrap().settings.display =
            TraceDisplay::S11(S11Display::Impedance);
        state.workspace.selected_mut().unwrap().settings.cal = Cal::CalOff;
        state.workspace.frequencies_hz = vec![5000, 5000, 10000];
        state.workspace.x_view.x_min = 10e6;
        state.workspace.x_view.x_max = 70e6;
        let mut config = AppConfig::from_state(&state);
        let encoded = toml::to_string(&config).unwrap();
        let restored: AppConfig = toml::from_str(&encoded).unwrap();
        let mut restarted = AppState::default();
        restored.apply_to(&mut restarted);
        assert_eq!(
            restarted.workspace.plan().unwrap(),
            state.workspace.plan().unwrap()
        );
        assert_eq!(
            restarted.workspace.frequencies_hz,
            state.workspace.frequencies_hz
        );
        assert_eq!(restarted.workspace.range, state.workspace.range);
        assert_eq!(restarted.workspace.x_view.x_min, 10e6);
        assert_eq!(restarted.workspace.x_view.x_max, 70e6);
        assert!(!restarted.any_running());
        assert!(
            restarted
                .workspace
                .traces
                .iter()
                .all(|trace| trace.completed.is_none())
        );
        for invalid in [
            Vec::new(),
            vec![kcsdi_core::segments::Segment {
                start_hz: 0,
                stop_hz: 1,
                max_step_hz: 0,
            }],
            vec![plan.segments()[0].definition(); 33],
        ] {
            config.workspace.segments = invalid;
            config.apply_to(&mut restarted);
            assert_eq!(restarted.workspace.sweep_mode, SweepMode::Segments);
            assert!(restarted.workspace.plan().is_err());
            assert!(!restarted.any_running());
        }
    }

    #[test]
    fn unknown_transport_tags_are_not_silently_changed_to_tcp() {
        for text in [
            "[connection]\nkind = \"bluetooth\"\nhost = \"instrument.local\"\n",
            "[[desktop.profiles]]\nname = \"Unknown\"\nkind = \"bluetooth\"\nhost = \"instrument.local\"\n",
        ] {
            assert!(toml::from_str::<AppConfig>(text).is_err());
        }
        let config: AppConfig = toml::from_str(
            "version = 2\n[connection]\nhost = \"instrument.local\"\n[desktop]\nprofiles = []\n",
        )
        .unwrap();
        let mut state = AppState::default();
        config.apply_to(&mut state);
        assert!(state.desktop.settings.profiles.is_empty());
    }

    #[test]
    fn legacy_connection_becomes_one_profile_without_changing_sweeps() {
        for version in ["", "version = 1\n"] {
            let text = format!(
                "{version}mode = \"s11\"\n[connection]\nhost = \"bench.example.invalid\"\nport = 4321\n[s11]\npoints = 401\n"
            );
            let config: AppConfig = toml::from_str(&text).unwrap();
            let mut state = AppState::default();
            config.apply_to(&mut state);
            assert_eq!(
                state.desktop.settings.profiles,
                vec![DeviceProfile {
                    name: "bench.example.invalid".into(),
                    target: kcsdi_core::connection::ConnectionTarget::Tcp {
                        host: "bench.example.invalid".into(),
                        port: 4321
                    },
                }]
            );
            assert_eq!(state.workspace.range.points, 401);
            assert_eq!(
                state.workspace.selected().unwrap().settings.display.mode(),
                AppMode::S11
            );
            assert_eq!(state.workspace.traces.len(), 2);
            assert!(!state.workspace.traces[0].settings.visible);
            assert!(state.workspace.traces[1].settings.visible);
            let saved = AppConfig::from_state(&state);
            assert_eq!(saved.version, CONFIG_VERSION);
            let mut restored = AppState::default();
            saved.apply_to(&mut restored);
            assert_eq!(
                restored.desktop.settings.profiles,
                state.desktop.settings.profiles
            );
        }
    }

    #[test]
    fn deleting_all_profiles_does_not_reimport_last_connection() {
        let config: AppConfig =
            toml::from_str("[connection]\nhost = \"bench.example.invalid\"\n").unwrap();
        let mut state = AppState::default();
        config.apply_to(&mut state);
        state.desktop.settings.profiles.clear();
        let saved = AppConfig::from_state(&state);
        let parsed: AppConfig = toml::from_str(&toml::to_string_pretty(&saved).unwrap()).unwrap();
        let mut restored = AppState::default();
        parsed.apply_to(&mut restored);
        assert!(restored.desktop.settings.profiles.is_empty());
        assert_eq!(restored.target.to_string(), "bench.example.invalid:901");
    }

    #[test]
    fn profiles_and_theme_roundtrip_in_the_existing_config_file() {
        let mut config = AppConfig::default();
        config.desktop.profiles = vec![DeviceProfile {
            name: "Bench A".into(),
            target: kcsdi_core::connection::ConnectionTarget::Tcp {
                host: "analyzer.example.invalid".into(),
                port: 901,
            },
        }];
        config.desktop.theme = ThemeMode::Light;
        let parsed: AppConfig = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        let mut state = AppState::default();
        parsed.apply_to(&mut state);
        assert_eq!(AppConfig::from_state(&state), config);
        let unknown: AppConfig = toml::from_str("[desktop]\ntheme = \"future-theme\"\n").unwrap();
        assert_eq!(unknown.desktop.theme, ThemeMode::System);
    }

    #[test]
    fn language_tags_roundtrip_and_unknown_tags_keep_other_settings() {
        for language in LanguagePreference::ALL {
            let cfg = AppConfig {
                language,
                ..AppConfig::default()
            };
            let text = toml::to_string_pretty(&cfg).unwrap();
            let parsed: AppConfig = toml::from_str(&text).unwrap();
            assert_eq!(parsed, cfg);
            assert!(text.contains(match language {
                LanguagePreference::System => "language = \"system\"",
                LanguagePreference::English => "language = \"en\"",
                LanguagePreference::SimplifiedChinese => "language = \"zh-CN\"",
            }));
        }
        let cfg: AppConfig = toml::from_str(
            "language = \"future-language\"\n[connection]\nhost = \"example.invalid\"\n",
        )
        .unwrap();
        assert_eq!(cfg.language, LanguagePreference::System);
        assert_eq!(cfg.connection.to_string(), "example.invalid:901");
        let legacy: AppConfig = toml::from_str("").unwrap();
        assert_eq!(legacy.language, LanguagePreference::System);
    }

    #[test]
    fn system_preference_is_saved_instead_of_the_resolved_language() {
        for language in crate::i18n::Language::ALL {
            let state = AppState {
                language,
                ..AppState::default()
            };
            let config = AppConfig::from_state(&state);
            assert_eq!(config.language, LanguagePreference::System);
            let text = toml::to_string_pretty(&config).unwrap();
            assert!(text.contains("language = \"system\""));
        }
    }

    #[test]
    fn partial_file_uses_defaults() {
        let cfg: AppConfig = toml::from_str("[connection]\nhost = \"example.invalid\"\n").unwrap();
        assert_eq!(cfg.connection.to_string(), "example.invalid:901");
        assert_eq!(cfg.spec.points, 201);
        // Sections absent from old config files fall back to defaults.
        assert_eq!(cfg.mode, "spec");
        assert_eq!(cfg.s11.start_hz, 1e6);
        assert_eq!(cfg.s11.stop_hz, 1000e6);
        assert_eq!(cfg.s11.points, 201);
        assert_eq!(cfg.s11.cal, "caloff");
        assert_eq!(cfg.s11.display, "return_loss");
        assert!(!cfg.s11.log_x);
        assert_eq!(cfg.s11.rbw, None);
    }

    #[test]
    fn s11_fields_roundtrip() {
        let text = r#"
version = 1
mode = "s11"

[s11]
start_hz = 2000000.0
stop_hz = 900000000.0
points = 401
cal = "calon"
display = "smith"
log_x = true
rbw = "30k"
"#;
        let cfg: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.mode, "s11");
        assert_eq!(cfg.s11.start_hz, 2e6);
        assert_eq!(cfg.s11.stop_hz, 900e6);
        assert_eq!(cfg.s11.points, 401);
        assert_eq!(cfg.s11.cal, "calon");
        assert_eq!(cfg.s11.display, "smith");
        assert!(cfg.s11.log_x);
        assert_eq!(cfg.s11.rbw.as_deref(), Some("30k"));
        let mut state = AppState::default();
        cfg.apply_to(&mut state);
        let saved = AppConfig::from_state(&state);
        let text = toml::to_string_pretty(&saved).unwrap();
        assert!(!text.contains("[s11]"));
        assert!(!text.contains("[spec]"));
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(saved, parsed);
        let mut restored = AppState::default();
        parsed.apply_to(&mut restored);
        assert_eq!(restored.workspace.range, state.workspace.range);
        assert_eq!(
            restored.workspace.selected().unwrap().settings,
            state.workspace.selected().unwrap().settings
        );
        assert!(restored.workspace.log_x);
    }

    #[test]
    fn env_override_wins() {
        assert_eq!(
            config_path_with_override(Some("test-config.toml")),
            Some(PathBuf::from("test-config.toml"))
        );
        assert_eq!(
            config_path_with_override(Some("")),
            config_path_with_override(None)
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn state_roundtrip() {
        let mut state = AppState::default();
        state.set_language_preference(LanguagePreference::SimplifiedChinese);
        state.target = kcsdi_core::connection::ConnectionTarget::Tcp {
            host: "analyzer.example.invalid".into(),
            port: 5025,
        };
        state.workspace.range = SweepRange::new(10e6, 400e6, 101);
        state.workspace.log_x = true;
        state
            .workspace
            .add_trace(TraceSettings {
                display: TraceDisplay::S11(S11Display::Vswr),
                cal: Cal::CalUser,
                rbw: Rbw::R1k,
                line_width: 2.5,
                color: egui::Color32::RED,
                impedance_visible: [false, true, false],
                ..Default::default()
            })
            .unwrap();
        state.workspace.traces[0].settings.lo = Lo::LowLo;
        state.workspace.traces[0].settings.visible = false;
        state.sweep = crate::state::SweepState::Running;
        let cfg = AppConfig::from_state(&state);
        let mut restored = AppState::default();
        cfg.apply_to(&mut restored);
        assert_eq!(restored.language, crate::i18n::Language::SimplifiedChinese);
        assert_eq!(
            restored.language_preference,
            LanguagePreference::SimplifiedChinese
        );
        assert_eq!(restored.target.to_string(), "analyzer.example.invalid:5025");
        assert_eq!(restored.workspace.range, state.workspace.range);
        assert!(restored.workspace.log_x);
        assert_eq!(restored.workspace.selected, state.workspace.selected);
        assert_eq!(restored.workspace.next_id, state.workspace.next_id);
        assert_eq!(restored.workspace.traces.len(), 2);
        for (original, restored) in state
            .workspace
            .traces
            .iter()
            .zip(&restored.workspace.traces)
        {
            assert_eq!(original.id, restored.id);
            assert_eq!(original.settings, restored.settings);
            assert!(restored.completed.is_none());
            assert!(restored.preview.is_none());
        }
        assert_eq!(restored.sweep, crate::state::SweepState::Idle);
        assert!(restored.active_plan.is_none());
    }

    #[test]
    fn legacy_y_log_setting_does_not_enable_frequency_log() {
        let cfg: AppConfig = toml::from_str("[s11]\nlog_y = true\n").unwrap();
        assert!(!cfg.spec.log_x);
        assert!(!cfg.s11.log_x);
        let mut state = AppState::default();
        cfg.apply_to(&mut state);
        let upgraded = AppConfig::from_state(&state);
        let saved = toml::to_string_pretty(&upgraded).unwrap();
        assert!(
            upgraded
                .workspace
                .traces
                .iter()
                .all(|trace| trace.y_view.is_none_or(|view| !view.log_y))
        );
        let parsed: AppConfig = toml::from_str(&saved).unwrap();
        assert_eq!(upgraded, parsed);
    }

    #[test]
    fn recording_settings_round_trip_without_resuming_the_run() {
        use crate::run_settings::{IntervalUnit, RecordingFormat, Retention, RunProgress};
        let mut state = AppState::default();
        let directory = tempfile::tempdir().unwrap();
        state.workspace.run.interval_ms = 120_000;
        state.workspace.run.interval_unit = IntervalUnit::Minutes;
        state.workspace.run.recording.enabled = true;
        state.workspace.run.recording.directory = directory.path().to_path_buf();
        state.workspace.run.recording.format = RecordingFormat::Csv;
        state.workspace.run.recording.retention = Retention::KeepLast(3);
        state.send(crate::state::WorkerCommand::RunWorkspace(
            state.workspace.plan().unwrap(),
        ));
        state.run_progress = Some(RunProgress::Saving);
        state.last_recording = Some((7, directory.path().join("old.csv")));
        let text = toml::to_string_pretty(&AppConfig::from_state(&state)).unwrap();
        let config: AppConfig = toml::from_str(&text).unwrap();
        let mut restored = AppState::default();
        config.apply_to(&mut restored);
        assert_eq!(restored.workspace.run, state.workspace.run);
        assert_eq!(
            restored.connection,
            crate::state::ConnectionState::Disconnected
        );
        assert_eq!(restored.sweep, crate::state::SweepState::Idle);
        assert!(restored.active_plan.is_none());
        assert!(restored.run_progress.is_none());
        assert!(restored.last_recording.is_none());
        assert!(!restored.workspace.run_editor.is_pending());
        assert_eq!(directory.path().read_dir().unwrap().count(), 0);
        assert!(!text.contains("old.csv"));
        assert!(!text.contains("run_progress"));
    }

    #[test]
    fn older_configs_default_to_continuous_acquisition_without_recording() {
        let config: AppConfig = toml::from_str("version = 5\n[workspace]\npoints = 201\n").unwrap();
        let mut state = AppState::default();
        config.apply_to(&mut state);
        assert_eq!(state.workspace.run, Default::default());
        assert!(!state.workspace.run.recording.enabled);
        assert_eq!(state.workspace.run.interval_ms, 0);
    }

    #[test]
    fn source_settings_restore_without_resuming_output_or_recording() {
        use crate::source_panel::{InstrumentFunction, SourcePending};
        use kcsdi_core::source::{SourceKind, SourceOutputState, SourceReport};
        let mut state = AppState {
            function: InstrumentFunction::AfSource,
            ..Default::default()
        };
        state.source.config.af.frequency_hz = 123_456;
        state.source.config.af.amplitude_mv = 1_234;
        state.source.config.af.modulation = "pm".into();
        state.source.config.af.pm_phase_deg = -123;
        state.source.pending = Some(SourcePending::Start);
        state.source.requested = Some(state.source.config.af.params(SourceKind::Af).unwrap());
        state.source.report = SourceReport {
            state: SourceOutputState::Requested(SourceKind::Af),
            warning: None,
        };
        let config = AppConfig::from_state(&state);
        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("pending"));
        assert!(!text.contains("requested"));
        assert!(!text.contains("report"));
        let loaded: AppConfig = toml::from_str(&text).unwrap();
        let mut restored = AppState::default();
        loaded.apply_to(&mut restored);
        assert_eq!(restored.function, state.function);
        assert_eq!(restored.source.config, state.source.config);
        assert_eq!(restored.source.report.state, SourceOutputState::NotStarted);
        assert!(restored.source.pending.is_none());
        assert!(restored.source.requested.is_none());
        assert!(!restored.any_running());
        assert!(restored.active_plan.is_none());
        let old: AppConfig = toml::from_str("version = 6").unwrap();
        old.apply_to(&mut restored);
        assert_eq!(restored.function, InstrumentFunction::Measurements);
        assert_eq!(restored.source.config, Default::default());
    }

    #[test]
    fn calibration_runtime_and_write_consent_are_never_restored() {
        use kcsdi_core::calibration::{CalibrationParams, CalibrationPhase};
        let mut state = AppState::default();
        state.calibration.begin(CalibrationParams::S11System);
        state.calibration.consent = true;
        let text = toml::to_string(&AppConfig::from_state(&state)).unwrap();
        assert!(!text.contains("calibration"));
        assert!(!text.contains("consent"));
        let loaded: AppConfig = toml::from_str(&text).unwrap();
        loaded.apply_to(&mut state);
        assert_eq!(state.calibration.report.phase, CalibrationPhase::NotStarted);
        assert!(!state.calibration.busy());
        assert!(!state.calibration.open);
        assert!(!state.calibration.consent);
        assert!(state.calibration.frozen.is_none());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn invalid_strings_fall_back_to_defaults() {
        let mut cfg = AppConfig::default();
        cfg.version = 1;
        cfg.mode = "bogus".to_string();
        cfg.s11.cal = "bogus".to_string();
        cfg.s11.display = "bogus".to_string();
        cfg.s11.rbw = Some("bogus".to_string());
        let mut state = AppState::default();
        cfg.apply_to(&mut state);
        assert_eq!(
            state.workspace.selected().unwrap().settings.display,
            TraceDisplay::Spec
        );
        let s11 = &state.workspace.traces[1].settings;
        assert_eq!(s11.cal, Cal::CalOff);
        assert_eq!(s11.display, TraceDisplay::S11(S11Display::ReturnLoss));
        assert_eq!(s11.rbw, Rbw::R10k);
    }

    #[test]
    fn version_two_migration_preserves_deleted_profiles_and_explicit_bandwidth() {
        for mode in ["spec", "s11"] {
            let text = format!(
                "version = 2\nmode = \"{mode}\"\n[connection]\nhost = \"bench.example.invalid\"\n"
            );
            let config: AppConfig = toml::from_str(&text).unwrap();
            let mut state = AppState::default();
            config.apply_to(&mut state);
            assert!(state.desktop.settings.profiles.is_empty());
            assert_eq!(state.workspace.traces.len(), 2);
            assert_eq!(
                state
                    .workspace
                    .traces
                    .iter()
                    .filter(|trace| trace.settings.visible)
                    .count(),
                1
            );
            assert_eq!(
                state.workspace.selected().unwrap().settings.display.mode(),
                parse_mode(mode)
            );
            assert_eq!(state.workspace.traces[1].settings.rbw, Rbw::R10k);
            assert_eq!(state.workspace.plan().unwrap().groups.len(), 1);
        }
    }

    #[test]
    fn empty_workspace_stays_empty_and_deleted_ids_are_not_reused_after_restart() {
        let mut state = AppState::default();
        let id = state.workspace.selected.unwrap();
        state.workspace.remove_trace(id);
        let config = AppConfig::from_state(&state);
        let parsed: AppConfig = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        let mut restored = AppState::default();
        parsed.apply_to(&mut restored);
        assert!(restored.workspace.traces.is_empty());
        assert!(restored.workspace.selected.is_none());
        assert!(
            restored
                .workspace
                .add_trace(TraceSettings::default())
                .unwrap()
                .0
                > id.0
        );
    }

    #[test]
    fn malformed_trace_identities_cannot_collide_or_exceed_the_state_limit() {
        let mut config = WorkspaceConfig {
            traces: Vec::new(),
            next_id: 0,
            selected: Some(TraceId(999)),
            ..Default::default()
        };
        for id in [0, 3, 3, u64::MAX, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11] {
            config.traces.push(TraceConfig {
                id: TraceId(id),
                ..Default::default()
            });
        }
        let workspace = config.restore();
        assert_eq!(workspace.traces.len(), MAX_TRACES);
        assert_eq!(workspace.selected, Some(TraceId(10)));
        assert_eq!(workspace.next_id, 11);
        assert!(workspace.plan().is_ok());
    }

    #[test]
    fn all_projections_and_unknown_current_values_have_stable_mappings() {
        for display in S11Display::ALL {
            assert_eq!(parse_display(display_as_str(display)), display);
        }
        let config = TraceConfig {
            display: "future-display".into(),
            cal: "future-cal".into(),
            rbw: "future-rbw".into(),
            lo: "future-lo".into(),
            line_width: f32::NAN,
            ..Default::default()
        };
        assert_eq!(config.settings(), TraceSettings::default());
    }

    #[test]
    fn distinct_legacy_ranges_are_archived_without_overriding_the_shared_range() {
        let legacy: AppConfig = toml::from_str(
            r#"
version = 2
mode = "s11"
[spec]
start_hz = 500000.0
stop_hz = 900000000.0
points = 101
log_x = true
[s11]
start_hz = 5000.0
stop_hz = 650000000.0
points = 201
log_x = false
"#,
        )
        .unwrap();
        let mut state = AppState::default();
        legacy.apply_to(&mut state);
        assert_eq!(state.workspace.range, SweepRange::new(5000.0, 650e6, 201));
        state.workspace.range = SweepRange::new(1e6, 2e6, 401);
        let saved = AppConfig::from_state(&state);
        let parsed: AppConfig = toml::from_str(&toml::to_string_pretty(&saved).unwrap()).unwrap();
        let mut restored = AppState::default();
        parsed.apply_to(&mut restored);
        assert_eq!(restored.workspace.range, state.workspace.range);
        let archive = restored.legacy_sweeps.unwrap();
        assert_eq!(archive.mode, "s11");
        assert_eq!(archive.spec, legacy.spec);
        assert_eq!(archive.s11, legacy.s11);
    }
}
