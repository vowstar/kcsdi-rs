// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Spectrum receiver fields independent of the displayed Y scale.

use super::sweep_controls::group_heading;
use crate::i18n::{Language, Text};
use crate::state::DEVICE_MODEL;
use crate::workspace::TraceSettings;
use kcsdi_core::commands::Lo;

pub fn receiver_fields(ui: &mut egui::Ui, settings: &mut TraceSettings, language: Language) {
    group_heading(ui, language.text(Text::LocalOscillator));
    ui.horizontal(|ui| {
        ui.selectable_value(&mut settings.lo, Lo::HighLo, language.text(Text::HighLo));
        ui.selectable_value(&mut settings.lo, Lo::LowLo, language.text(Text::LowLo));
    });
    group_heading(ui, language.text(Text::RefLevel));
    let caps = DEVICE_MODEL.capabilities();
    ui.add(
        egui::DragValue::new(&mut settings.ref_level_dbm)
            .range(caps.spec.ref_min_dbm..=caps.spec.ref_max_dbm)
            .clamp_existing_to_range(false)
            .suffix(" dBm"),
    );
}
