// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Theme, fonts, and design tokens.
//!
//! Compact desktop palettes, fixed trace colors and monospace readouts.

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use serde::{Deserialize, Serialize};

use crate::i18n::{Language, Text};

/// The system option follows desktop appearance changes while running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    #[serde(other)]
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::System => Text::ThemeSystem,
            Self::Light => Text::ThemeLight,
            Self::Dark => Text::ThemeDark,
        })
    }
}

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
    egui::Color32::from_rgb(0xf4, 0xdf, 0x45),
    egui::Color32::from_rgb(0x5b, 0xdc, 0x76),
    egui::Color32::from_rgb(0xf3, 0x6c, 0xa5),
    egui::Color32::from_rgb(0xf2, 0xc1, 0x82),
    egui::Color32::from_rgb(0xd8, 0x8c, 0xdb),
    egui::Color32::from_rgb(0x8e, 0xce, 0xf0),
    egui::Color32::from_rgb(0xff, 0xa0, 0x52),
    egui::Color32::from_rgb(0x5b, 0xd5, 0xc6),
    egui::Color32::from_rgb(0x99, 0xb4, 0xf3),
    egui::Color32::from_rgb(0xc1, 0xa0, 0xef),
];

const LIGHT_TRACE_COLORS: [egui::Color32; 10] = [
    egui::Color32::from_rgb(0x8a, 0x74, 0x00),
    egui::Color32::from_rgb(0x24, 0x84, 0x39),
    egui::Color32::from_rgb(0xc0, 0x28, 0x68),
    egui::Color32::from_rgb(0x9d, 0x5e, 0x00),
    egui::Color32::from_rgb(0x94, 0x3a, 0x96),
    egui::Color32::from_rgb(0x16, 0x76, 0xa4),
    egui::Color32::from_rgb(0xaf, 0x56, 0x0b),
    egui::Color32::from_rgb(0x00, 0x82, 0x77),
    egui::Color32::from_rgb(0x38, 0x66, 0xb7),
    egui::Color32::from_rgb(0x77, 0x48, 0xad),
];

pub fn trace_colors(dark: bool) -> &'static [egui::Color32; 10] {
    if dark {
        &TRACE_COLORS
    } else {
        &LIGHT_TRACE_COLORS
    }
}

/// CJK glyphs the default egui fonts do not cover. Embedded once and
/// registered at the lowest priority so Latin text keeps the default faces.
const CJK_FALLBACK: &[u8] = include_bytes!("../assets/fonts/DroidSansFallback.ttf");

/// Apply fonts and visuals. Called once at startup.
pub fn setup(ctx: &egui::Context) {
    install_cjk_fallback(ctx);

    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let mut style = (*ctx.style_of(theme)).clone();
        style.visuals = visuals(theme == egui::Theme::Dark);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.menu_spacing = 4.0;
        for (text_style, size) in [
            (egui::TextStyle::Body, 13.0),
            (egui::TextStyle::Button, 12.5),
            (egui::TextStyle::Monospace, 13.0),
            (egui::TextStyle::Small, 11.5),
        ] {
            style
                .text_styles
                .insert(text_style, egui::FontId::monospace(size));
        }
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(19.0));
        ctx.set_style_of(theme, style);
    }
    apply(ctx, ThemeMode::default());
}

/// Select a preinstalled palette without reinstalling fonts or styles.
pub fn apply(ctx: &egui::Context, theme: ThemeMode) {
    ctx.set_theme(match theme {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    });
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

fn visuals(dark: bool) -> egui::Visuals {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let primary = if dark {
        PRIMARY
    } else {
        egui::Color32::from_rgb(0x19, 0x78, 0xb6)
    };

    // Primary accent on selection, hyperlinks, and the emphasized
    // (active/selected) widget states.
    visuals.hyperlink_color = primary;
    visuals.selection.bg_fill = primary;
    visuals.selection.stroke.color = egui::Color32::WHITE;
    visuals.widgets.active.weak_bg_fill = primary;
    visuals.widgets.active.bg_fill = primary;
    visuals.widgets.hovered.weak_bg_fill = primary.gamma_multiply(0.2);

    // Semantic foreground colors from the reference palette.
    visuals.error_fg_color = ERROR;
    visuals.warn_fg_color = WARN;

    if dark {
        visuals.window_fill = egui::Color32::from_gray(24);
        visuals.panel_fill = egui::Color32::from_gray(20);
        visuals.extreme_bg_color = egui::Color32::from_gray(12);
        visuals.faint_bg_color = egui::Color32::from_gray(30);
    } else {
        visuals.window_fill = egui::Color32::WHITE;
        visuals.panel_fill = egui::Color32::from_rgb(0xf5, 0xf6, 0xf7);
        visuals.extreme_bg_color = egui::Color32::WHITE;
        visuals.faint_bg_color = egui::Color32::from_rgb(0xeb, 0xef, 0xf2);
        visuals.warn_fg_color = egui::Color32::from_rgb(0xa3, 0x59, 0x00);
        // Active foreground also colors strong labels and pressed radio text.
        // Keep the light palette's black foreground readable on both surfaces.
        let pressed_fill = egui::Color32::from_rgb(0xd3, 0xe9, 0xf6);
        visuals.widgets.active.weak_bg_fill = pressed_fill;
        visuals.widgets.active.bg_fill = pressed_fill;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(135));
    }
    visuals.window_shadow = egui::epaint::Shadow::NONE;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, Text};

    #[test]
    fn palettes_keep_density_and_use_distinct_backgrounds_and_trace_colors() {
        let ctx = egui::Context::default();
        setup(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(style.visuals.dark_mode, theme == egui::Theme::Dark);
            assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 13.0);
            assert_eq!(style.text_styles[&egui::TextStyle::Button].size, 12.5);
            assert_eq!(
                style.visuals.window_corner_radius,
                egui::CornerRadius::same(4)
            );
        }
        assert_ne!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            ctx.style_of(egui::Theme::Light).visuals.panel_fill
        );
        assert_ne!(trace_colors(true), trace_colors(false));
    }

    #[test]
    fn appearance_preference_can_return_to_system_after_manual_selection() {
        let ctx = egui::Context::default();
        setup(&ctx);
        for (mode, preference) in [
            (ThemeMode::Light, egui::ThemePreference::Light),
            (ThemeMode::Dark, egui::ThemePreference::Dark),
            (ThemeMode::System, egui::ThemePreference::System),
        ] {
            apply(&ctx, mode);
            assert_eq!(ctx.options(|options| options.theme_preference), preference);
        }
    }

    #[test]
    fn strong_labels_and_radio_states_remain_readable_in_both_palettes() {
        for dark in [false, true] {
            let visuals = visuals(dark);
            for background in [visuals.panel_fill, visuals.window_fill] {
                assert!(contrast(visuals.strong_text_color(), background) >= 7.0);
                for widget in [visuals.widgets.inactive, visuals.widgets.active] {
                    assert!(contrast(widget.text_color(), background) >= 4.5);
                    assert!(contrast(widget.fg_stroke.color, widget.bg_fill) >= 3.0);
                }
            }
            if !dark {
                assert!(
                    contrast(visuals.widgets.inactive.bg_stroke.color, visuals.panel_fill,) >= 3.0
                );
            }
        }
    }

    fn contrast(first: egui::Color32, second: egui::Color32) -> f32 {
        fn luminance(color: egui::Color32) -> f32 {
            let linear = [color.r(), color.g(), color.b()].map(|value| {
                let value = f32::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            });
            linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
        }

        let first = luminance(first);
        let second = luminance(second);
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    #[test]
    fn embedded_font_covers_every_translation_in_both_ui_families() {
        let ctx = egui::Context::default();
        setup(&ctx);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.fonts_mut(|fonts| {
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    let mut font = fonts.fonts.font(&family);
                    // Inspect the character map directly. egui 0.36's
                    // has_glyph can reject glyphs sharing the fallback face.
                    let characters = font.characters();
                    for language in Language::ALL {
                        let strings = std::iter::once(language.label())
                            .chain(Text::ALL.iter().map(|&key| language.text(key)))
                            .chain(std::iter::once("中文 日本語 한국어"));
                        for text in strings {
                            for character in text.chars() {
                                assert!(
                                    characters.contains_key(&character),
                                    "{family:?}: {language:?}: U+{:04X}",
                                    character as u32
                                );
                            }
                        }
                    }
                }
            });
        });
        output.drop_without_applying_deltas();
    }
}
