// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! GUI application for KC901 series instruments.
//!
//! Provides a graphical interface for controlling KC901 analyzers using
//! egui/eframe, modeled after the KCSDI reference interface.

mod acquisition;
mod analysis_tools;
mod app;
mod calibration_panel;
mod config;
mod connection_editor;
mod desktop;
mod device_lookup;
mod device_worker;
mod export;
mod folder_opener;
mod frequency_editor;
mod frequency_list;
mod health;
mod i18n;
mod panels;
mod preview;
mod recording;
mod run_settings;
mod segmented;
mod source_panel;
mod spreadsheet;
mod state;
mod theme;
mod widgets;
mod workspace;

#[cfg(test)]
mod list_tests;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let viewport = egui::ViewportBuilder::default()
        .with_app_id("io.github.vowstar.kcsdi-gui")
        .with_inner_size([1280.0, 850.0])
        .with_min_inner_size([960.0, 600.0]);

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "kcsdi",
        options,
        Box::new(|cc| Ok(Box::new(app::KcsdiApp::new(cc)))),
    )
}
