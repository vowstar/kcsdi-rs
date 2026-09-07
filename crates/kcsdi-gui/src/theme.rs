// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Theme, fonts, and design tokens.
//!
//! Tokens follow the KCSDI reference interface (the reference UI analysis
//! section 4): dark theme, primary #4696d3, a fixed trace color sequence,
//! monospace numerals for readouts.

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

/// Accent color of the reference dark theme.
pub const PRIMARY: egui::Color32 = egui::Color32::from_rgb(0x46, 0x96, 0xd3);

/// Success/ok status color (reference semantic palette).
// Consumed by panels added separately.
#[allow(dead_code)]
pub const SUCCESS: egui::Color32 = egui::Color32::from_rgb(0x43, 0xa0, 0x47);

/// Warning status color (reference semantic palette).
pub const WARN: egui::Color32 = egui::Color32::from_rgb(0xff, 0x98, 0x00);

/// Error status color (reference semantic palette).
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(0xd3, 0x2f, 0x2f);

/// Info status color (reference semantic palette).
// Consumed by panels added separately.
#[allow(dead_code)]
pub const INFO: egui::Color32 = egui::Color32::from_rgb(0x21, 0x96, 0xf3);

/// Trace colors, drawn in order for multi-trace displays.
// Consumed by the chart widgets added separately.
#[allow(dead_code)]
pub const TRACE_COLORS: [egui::Color32; 10] = [
    egui::Color32::from_rgb(0xff, 0x8c, 0x00),
    egui::Color32::from_rgb(0xdc, 0x1d, 0x82),
    egui::Color32::from_rgb(0x6f, 0x49, 0xc0),
    egui::Color32::from_rgb(0xa9, 0xa9, 0x11),
    egui::Color32::from_rgb(0xb4, 0x00, 0x59),
    egui::Color32::from_rgb(0x46, 0x96, 0xd3),
    egui::Color32::from_rgb(0x43, 0xa0, 0x47),
    egui::Color32::from_rgb(0xff, 0x98, 0x00),
    egui::Color32::from_rgb(0x21, 0x96, 0xf3),
    egui::Color32::from_rgb(0xd3, 0x2f, 0x2f),
];

/// CJK glyphs the default egui fonts do not cover. Embedded once and
/// registered at the lowest priority so Latin text keeps the default faces.
const CJK_FALLBACK: &[u8] = include_bytes!("../assets/fonts/DroidSansFallback.ttf");

/// Apply fonts and visuals. Called once at startup.
pub fn setup(ctx: &egui::Context) {
    install_cjk_fallback(ctx);

    // egui 0.36 keeps one style per theme; the reference interface is
    // dark-only for v1, so both slots get the same dark style.
    let dark = ctx.style_of(egui::Theme::Dark);
    let mut style = egui::Style {
        visuals: visuals(),
        ..(*dark).clone()
    };
    // 8 px base spacing unit, flat 4 px widget geometry.
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.menu_spacing = 4.0;
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::Dark);
}

fn install_cjk_fallback(ctx: &egui::Context) {
    let families = vec![
        InsertFontFamily {
            family: egui::FontFamily::Proportional,
            priority: FontPriority::Lowest,
        },
        InsertFontFamily {
            family: egui::FontFamily::Monospace,
            priority: FontPriority::Lowest,
        },
    ];
    ctx.add_font(FontInsert {
        name: "cjk_fallback".into(),
        data: egui::FontData::from_static(CJK_FALLBACK),
        families,
    });
}

/// Dark visuals matching the reference theme tokens.
fn visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();

    // Primary accent on selection, hyperlinks, and the emphasized
    // (active/selected) widget states.
    visuals.hyperlink_color = PRIMARY;
    visuals.selection.bg_fill = PRIMARY;
    visuals.selection.stroke.color = egui::Color32::WHITE;
    visuals.widgets.active.weak_bg_fill = PRIMARY;
    visuals.widgets.active.bg_fill = PRIMARY;
    visuals.widgets.hovered.weak_bg_fill = PRIMARY.gamma_multiply(0.3);

    // Semantic foreground colors from the reference palette.
    visuals.error_fg_color = ERROR;
    visuals.warn_fg_color = WARN;

    // Near-black window chrome, per the chart chrome tokens (#111 mask).
    let window_bg = egui::Color32::from_rgb(0x14, 0x14, 0x14);
    visuals.window_fill = window_bg;
    visuals.panel_fill = window_bg;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x0c, 0x0c, 0x0c);
    visuals.faint_bg_color = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e);

    // 4 px rounding everywhere, matching the reference shape token.
    let radius = egui::CornerRadius::same(4);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open.corner_radius = radius;
    visuals.window_corner_radius = radius;
    visuals.menu_corner_radius = radius;

    visuals
}
