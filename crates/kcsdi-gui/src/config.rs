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

use crate::state::AppState;

/// Environment variable that overrides the config file path.
pub const ENV_CONFIG_PATH: &str = "KCSDI_CONFIG_PATH";

/// Current config schema version.
pub const CONFIG_VERSION: u32 = 1;

/// Persisted user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub connection: Connection,
    pub spec: Spec,
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

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            connection: Connection::default(),
            spec: Spec::default(),
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

impl AppConfig {
    /// Snapshot the persisted fields of the current UI state.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            version: CONFIG_VERSION,
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
        }
    }

    /// Apply loaded settings to freshly initialized UI state.
    pub fn apply_to(&self, state: &mut AppState) {
        state.host = self.connection.host.clone();
        state.port = self.connection.port;
        state.spec.start_hz = self.spec.start_hz;
        state.spec.stop_hz = self.spec.stop_hz;
        state.spec.points = self.spec.points;
        if let Ok(rbw) = self.spec.rbw.parse() {
            state.spec.rbw = rbw;
        }
        state.spec.ref_level_dbm = self.spec.ref_level_dbm;
        state.spec.start_stop_changed();
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
    fn state_roundtrip() {
        let mut state = AppState::default();
        state.host = "analyzer.example.invalid".to_string();
        state.port = 5025;
        let cfg = AppConfig::from_state(&state);
        let mut restored = AppState::default();
        cfg.apply_to(&mut restored);
        assert_eq!(restored.host, "analyzer.example.invalid");
        assert_eq!(restored.port, 5025);
        assert_eq!(restored.spec.start_hz, state.spec.start_hz);
        assert_eq!(restored.spec.center_hz, state.spec.center_hz);
    }
}
