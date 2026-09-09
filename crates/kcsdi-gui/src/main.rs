// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! GUI application for KC901 series instruments.
//!
//! Provides a graphical interface for controlling KC901 analyzers using
//! egui/eframe, modeled after the KCSDI reference interface.

mod analysis_tools;
mod app;
mod config;
mod desktop;
mod device_worker;
mod export;
mod i18n;
mod panels;
mod preview;
mod state;
mod theme;
mod widgets;

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
