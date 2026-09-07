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

use kcsdi_core::commands::Cal;
use kcsdi_core::model::Rbw;

use crate::state::{AppMode, AppState, S11Display};

/// Environment variable that overrides the config file path.
pub const ENV_CONFIG_PATH: &str = "KCSDI_CONFIG_PATH";

/// Current config schema version.
pub const CONFIG_VERSION: u32 = 1;

/// Persisted user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    /// Last-used function mode ("spec" or "s11").
    pub mode: String,
    pub connection: Connection,
    pub spec: Spec,
    pub s11: S11,
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
    /// Logarithmic Y axis for cartesian displays.
    pub log_y: bool,
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
    }
}

fn parse_display(s: &str) -> S11Display {
    match s {
        "phase" => S11Display::Phase,
        "vswr" => S11Display::Vswr,
        "smith" => S11Display::Smith,
        "impedance" => S11Display::Impedance,
        _ => S11Display::ReturnLoss,
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            mode: mode_as_str(AppMode::default()).to_string(),
            connection: Connection::default(),
            spec: Spec::default(),
            s11: S11::default(),
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
            log_y: false,
            rbw: None,
        }
    }
}

impl AppConfig {
    /// Snapshot the persisted fields of the current UI state.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            version: CONFIG_VERSION,
            mode: mode_as_str(state.mode).to_string(),
            connection: Connection {
                host: state.host.clone(),
                port: state.port,
            },
            spec: Spec {
                start_hz: state.spec.start_hz,
                stop_hz: state.spec.stop_hz,
                points: state.spec.points,
                rbw: state.spec.rbw.as_str().to_string(),
                ref_level_dbm: state.spec.ref_level_dbm,
            },
            s11: S11 {
                start_hz: state.s11.start_hz,
                stop_hz: state.s11.stop_hz,
                points: state.s11.points,
                cal: state.s11.cal.as_str().to_string(),
                display: display_as_str(state.s11.display).to_string(),
                log_y: state.s11.log_y,
                rbw: state.s11.rbw.map(|rbw| rbw.as_str().to_string()),
            },
        }
    }

    /// Apply loaded settings to freshly initialized UI state. Unknown
    /// strings fall back to defaults instead of failing.
    pub fn apply_to(&self, state: &mut AppState) {
        state.host = self.connection.host.clone();
        state.port = self.connection.port;
        state.mode = parse_mode(&self.mode);
        state.spec.start_hz = self.spec.start_hz;
        state.spec.stop_hz = self.spec.stop_hz;
        state.spec.points = self.spec.points;
        if let Ok(rbw) = self.spec.rbw.parse() {
            state.spec.rbw = rbw;
        }
        state.spec.ref_level_dbm = self.spec.ref_level_dbm;
        state.spec.start_stop_changed();
        state.s11.start_hz = self.s11.start_hz;
        state.s11.stop_hz = self.s11.stop_hz;
        state.s11.points = self.s11.points;
        if let Ok(cal) = self.s11.cal.parse::<Cal>() {
            state.s11.cal = cal;
        }
        state.s11.display = parse_display(&self.s11.display);
        state.s11.log_y = self.s11.log_y;
        state.s11.rbw = self.s11.rbw.as_deref().and_then(|s| s.parse::<Rbw>().ok());
        state.s11.start_stop_changed();
    }
}

/// Resolve the config file path: env override first, then the
/// platform-standard config directory.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ENV_CONFIG_PATH)
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

    #[test]
    fn default_roundtrip() {
        let cfg = AppConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, parsed);
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
        assert!(!cfg.s11.log_y);
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
log_y = true
rbw = "30k"
"#;
        let cfg: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.mode, "s11");
        assert_eq!(cfg.s11.start_hz, 2e6);
        assert_eq!(cfg.s11.stop_hz, 900e6);
        assert_eq!(cfg.s11.points, 401);
        assert_eq!(cfg.s11.cal, "calon");
        assert_eq!(cfg.s11.display, "smith");
        assert!(cfg.s11.log_y);
        assert_eq!(cfg.s11.rbw.as_deref(), Some("30k"));
        // Serializing and parsing back preserves every field.
        let parsed: AppConfig = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn env_override_wins() {
        // SAFETY: single-threaded test binary section for this test only.
        unsafe { std::env::set_var(ENV_CONFIG_PATH, "/tmp/kcsdi-test-config.toml") };
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/tmp/kcsdi-test-config.toml"))
        );
        unsafe { std::env::remove_var(ENV_CONFIG_PATH) };
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn state_roundtrip() {
        let mut state = AppState::default();
        state.host = "analyzer.example.invalid".to_string();
        state.port = 5025;
        state.mode = AppMode::S11;
        state.s11.start_hz = 10e6;
        state.s11.stop_hz = 400e6;
        state.s11.points = 101;
        state.s11.cal = Cal::CalUser;
        state.s11.display = S11Display::Vswr;
        state.s11.log_y = true;
        state.s11.rbw = Some(Rbw::R1k);
        state.s11.start_stop_changed();
        let cfg = AppConfig::from_state(&state);
        let mut restored = AppState::default();
        cfg.apply_to(&mut restored);
        assert_eq!(restored.host, "analyzer.example.invalid");
        assert_eq!(restored.port, 5025);
        assert_eq!(restored.spec.start_hz, state.spec.start_hz);
        assert_eq!(restored.spec.center_hz, state.spec.center_hz);
        assert_eq!(restored.mode, AppMode::S11);
        assert_eq!(restored.s11.start_hz, state.s11.start_hz);
        assert_eq!(restored.s11.center_hz, state.s11.center_hz);
        assert_eq!(restored.s11.points, 101);
        assert_eq!(restored.s11.cal, Cal::CalUser);
        assert_eq!(restored.s11.display, S11Display::Vswr);
        assert!(restored.s11.log_y);
        assert_eq!(restored.s11.rbw, Some(Rbw::R1k));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn invalid_strings_fall_back_to_defaults() {
        let mut cfg = AppConfig::default();
        cfg.mode = "bogus".to_string();
        cfg.s11.cal = "bogus".to_string();
        cfg.s11.display = "bogus".to_string();
        cfg.s11.rbw = Some("bogus".to_string());
        let mut state = AppState::default();
        state.mode = AppMode::S11;
        state.s11.cal = Cal::CalOn;
        state.s11.display = S11Display::Smith;
        state.s11.rbw = Some(Rbw::R1k);
        cfg.apply_to(&mut state);
        assert_eq!(state.mode, AppMode::Spec);
        // Unparseable cal keeps the state's current value, like spec.rbw.
        assert_eq!(state.s11.cal, Cal::CalOn);
        assert_eq!(state.s11.display, S11Display::ReturnLoss);
        assert_eq!(state.s11.rbw, None);
    }
}
