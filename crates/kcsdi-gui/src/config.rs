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
use crate::desktop::{DesktopConfig, DeviceProfile};
use crate::i18n::LanguagePreference;
use crate::state::{AppMode, AppState, S11Display};
use crate::workspace::{SweepRange, TraceDisplay, TraceSettings, TraceState, Workspace};

/// Environment variable that overrides the config file path.
pub const ENV_CONFIG_PATH: &str = "KCSDI_CONFIG_PATH";

/// Current config schema version.
pub const CONFIG_VERSION: u32 = 3;

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
    pub connection: Connection,
    #[serde(skip_serializing)]
    pub spec: Spec,
    #[serde(skip_serializing)]
    pub s11: S11,
    /// Original fixed-mode settings kept for recovery, never used for acquisition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_sweeps: Option<LegacySweeps>,
    pub workspace: WorkspaceConfig,
    pub desktop: DesktopConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacySweeps {
    pub mode: String,
    pub spec: Spec,
    pub s11: S11,
}

/// Trace definitions only. Measurements are never written to user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: u32,
    pub log_x: bool,
    pub selected: Option<TraceId>,
    pub next_id: u64,
    pub traces: Vec<TraceConfig>,
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
        }
    }

    fn settings(&self) -> TraceSettings {
        let display = match self.display.as_str() {
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
        Self {
            start_hz: workspace.range.start_hz,
            stop_hz: workspace.range.stop_hz,
            points: workspace.range.points,
            log_x: workspace.log_x,
            selected: workspace.selected,
            next_id: workspace.next_id,
            traces: workspace
                .traces
                .iter()
                .map(|trace| TraceConfig::from_settings(trace.id, &trace.settings))
                .collect(),
        }
    }

    fn restore(&self) -> Workspace {
        let mut workspace =
            Workspace::empty(SweepRange::new(self.start_hz, self.stop_hz, self.points));
        workspace.log_x = self.log_x;
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
            workspace.traces.push(TraceState::new(
                trace.id,
                trace.settings(),
                &workspace.range,
            ));
        }
        workspace.next_id = workspace.next_id.max(self.next_id);
        workspace.selected = self
            .selected
            .filter(|id| workspace.traces.iter().any(|trace| trace.id == *id))
            .or_else(|| workspace.traces.last().map(|trace| trace.id));
        workspace
    }
}

/// Last-used connection target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    pub host: String,
    pub port: u16,
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
            connection: Connection::default(),
            spec: Spec::default(),
            s11: S11::default(),
            legacy_sweeps: None,
            workspace: WorkspaceConfig::default(),
            desktop: DesktopConfig::default(),
        }
    }
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 901,
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
            connection: Connection {
                host: state.host.clone(),
                port: state.port,
            },
            workspace: WorkspaceConfig::from_workspace(&state.workspace),
            desktop: state.desktop.settings.clone(),
            ..Self::default()
        }
    }

    /// Apply loaded settings to freshly initialized UI state. Unknown
    /// strings fall back to defaults instead of failing.
    pub fn apply_to(&self, state: &mut AppState) {
        state.desktop.settings = self.desktop.clone();
        // Import the legacy connection once. A version-2 empty profile list
        // represents the user's choice and must stay empty after deletion.
        if self.version < 2 && state.desktop.settings.profiles.is_empty() {
            let host = self.connection.host.trim();
            if !host.is_empty() {
                state.desktop.settings.profiles.push(DeviceProfile {
                    name: host.to_owned(),
                    host: host.to_owned(),
                    port: self.connection.port,
                });
            }
        }
        state.set_language_preference(self.language);
        state.host = self.connection.host.clone();
        state.port = self.connection.port;
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
    }

    fn legacy_workspace(&self) -> Workspace {
        let selected = parse_mode(&self.mode);
        let (range, log_x) = match selected {
            AppMode::Spec => (
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
            AppMode::Spec => spec,
            AppMode::S11 => s11,
        });
        workspace
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
    use crate::theme::ThemeMode;

    #[test]
    fn default_roundtrip() {
        let cfg = AppConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, parsed);
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
                    host: "bench.example.invalid".into(),
                    port: 4321,
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
        assert_eq!(restored.host, "bench.example.invalid");
    }

    #[test]
    fn profiles_and_theme_roundtrip_in_the_existing_config_file() {
        let mut config = AppConfig::default();
        config.desktop.profiles = vec![DeviceProfile {
            name: "Bench A".into(),
            host: "analyzer.example.invalid".into(),
            port: 901,
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
        assert_eq!(cfg.connection.host, "example.invalid");
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
        assert_eq!(cfg.connection.host, "example.invalid");
        assert_eq!(cfg.connection.port, 901);
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
        state.host = "analyzer.example.invalid".to_string();
        state.port = 5025;
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
        assert_eq!(restored.host, "analyzer.example.invalid");
        assert_eq!(restored.port, 5025);
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
        assert!(!saved.contains("log_y"));
        let parsed: AppConfig = toml::from_str(&saved).unwrap();
        assert_eq!(upgraded, parsed);
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
