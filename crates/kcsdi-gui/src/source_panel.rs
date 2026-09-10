// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Signal-source drafts, session-local reports and explicit output controls.

use kcsdi_core::source::{
    AF_FM_MAX_HZ, AFOUT_MAX_MV, Modulation, RF_ASK_DEPTHS, SourceAmplitude, SourceKind,
    SourceOutputState, SourceParams, SourcePort, SourceReport, SourceWarning,
    rf_fm_deviation_max_hz,
};
use serde::{Deserialize, Serialize};

use crate::i18n::{Language, Text};
use crate::state::{AppState, ConnectionState, DEVICE_MODEL, SweepState, WorkerCommand};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstrumentFunction {
    RfSource,
    AfSource,
    #[default]
    #[serde(other)]
    Measurements,
}

impl InstrumentFunction {
    pub const ALL: [Self; 3] = [Self::Measurements, Self::RfSource, Self::AfSource];

    pub fn kind(self) -> Option<SourceKind> {
        match self {
            Self::Measurements => None,
            Self::RfSource => Some(SourceKind::Rf),
            Self::AfSource => Some(SourceKind::Af),
        }
    }

    pub fn label(self, language: Language) -> &'static str {
        language.text(match self {
            Self::Measurements => Text::Measurements,
            Self::RfSource => Text::RfSource,
            Self::AfSource => Text::AfSource,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceDraft {
    pub frequency_hz: u64,
    pub port: String,
    pub amplitude_dbm: i32,
    pub amplitude_mv: u32,
    pub modulation: String,
    pub modulation_frequency_hz: u32,
    pub ask_depth_percent: u32,
    pub fm_deviation_hz: u32,
    pub pm_phase_deg: i32,
}

impl Default for SourceDraft {
    fn default() -> Self {
        Self {
            frequency_hz: 1_000_000,
            port: "port1".into(),
            amplitude_dbm: -30,
            amplitude_mv: 100,
            modulation: "off".into(),
            modulation_frequency_hz: 1_000,
            ask_depth_percent: 50,
            fm_deviation_hz: 1_000,
            pm_phase_deg: 0,
        }
    }
}

impl SourceDraft {
    pub fn params(&self, kind: SourceKind) -> kcsdi_core::Result<SourceParams> {
        let invalid = |detail: &str| kcsdi_core::Error::InvalidParameter(detail.into());
        let port = match self.port.as_str() {
            "port1" => SourcePort::Port1,
            "port2" => SourcePort::Port2,
            "afout" => SourcePort::AfOut,
            _ => return Err(invalid("choose a supported source output port")),
        };
        let params = SourceParams {
            kind,
            port,
            frequency_hz: self.frequency_hz,
            amplitude: if port == SourcePort::AfOut {
                SourceAmplitude::Millivolts(self.amplitude_mv)
            } else {
                SourceAmplitude::Dbm(self.amplitude_dbm)
            },
            modulation: self.modulation()?,
        };
        params.validate(&DEVICE_MODEL.capabilities())?;
        Ok(params)
    }

    fn modulation(&self) -> kcsdi_core::Result<Modulation> {
        let frequency_hz = self.modulation_frequency_hz;
        match self.modulation.as_str() {
            "off" => Ok(Modulation::Off),
            "ask" => Ok(Modulation::Ask {
                frequency_hz,
                depth_percent: self.ask_depth_percent,
            }),
            "fm" => Ok(Modulation::Fm {
                frequency_hz,
                deviation_hz: self.fm_deviation_hz,
            }),
            "pm" => Ok(Modulation::Pm {
                frequency_hz,
                phase_deg: self.pm_phase_deg,
            }),
            _ => Err(kcsdi_core::Error::InvalidParameter(
                "choose a supported modulation".into(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub rf: SourceDraft,
    pub af: SourceDraft,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            rf: SourceDraft::default(),
            af: SourceDraft {
                frequency_hz: 1_000,
                port: "afout".into(),
                ..Default::default()
            },
        }
    }
}

impl SourceConfig {
    pub fn draft(&self, kind: SourceKind) -> &SourceDraft {
        match kind {
            SourceKind::Rf => &self.rf,
            SourceKind::Af => &self.af,
        }
    }

    fn draft_mut(&mut self, kind: SourceKind) -> &mut SourceDraft {
        match kind {
            SourceKind::Rf => &mut self.rf,
            SourceKind::Af => &mut self.af,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcePending {
    Start,
    Stop,
}

#[derive(Default)]
pub struct SourceUi {
    pub config: SourceConfig,
    pub report: SourceReport,
    pub pending: Option<SourcePending>,
    pub requested: Option<SourceParams>,
}

impl SourceUi {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
            || matches!(
                self.report.state,
                SourceOutputState::Requested(_) | SourceOutputState::Unknown
            )
    }

    pub fn connected(&mut self) {
        if self.report.state != SourceOutputState::Unknown {
            self.report = SourceReport::default();
        }
        self.pending = None;
        self.requested = None;
    }

    pub fn lost(&mut self) {
        if self.busy() {
            self.report = SourceReport {
                state: SourceOutputState::Unknown,
                warning: None,
            };
        }
        self.pending = None;
    }

    pub fn accept(&mut self, report: SourceReport, function: InstrumentFunction) {
        if let SourceOutputState::Requested(kind) = report.state
            && (self.pending == Some(SourcePending::Stop)
                || function.kind() != Some(kind)
                || self.requested.is_none_or(|params| params.kind != kind))
        {
            return;
        }
        self.report = report;
        self.pending = None;
        if matches!(
            report.state,
            SourceOutputState::NotStarted | SourceOutputState::StopSent
        ) {
            self.requested = None;
        }
    }

    pub fn label(&self, language: Language) -> &'static str {
        language.text(match self.pending {
            Some(SourcePending::Start) => Text::SourceApplying,
            Some(SourcePending::Stop) => Text::Stopping,
            None => match self.report.state {
                SourceOutputState::NotStarted => Text::SourceNotStarted,
                SourceOutputState::Requested(_) => Text::SourceRequested,
                SourceOutputState::StopSent => Text::SourceStopSent,
                SourceOutputState::Unknown => Text::SourceUnknown,
            },
        })
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(kind) = state.function.kind() else {
        return;
    };
    let language = state.language;
    egui::ScrollArea::vertical()
        .id_salt("source_controls")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(state.function.label(language));
                ui.separator();
                ui.label(state.source.label(language));
                if state.source.pending.is_some() {
                    ui.spinner();
                }
            });
            if let Some(warning) = state.source.report.warning {
                ui.colored_label(
                    crate::theme::WARN,
                    language.text(match warning {
                        SourceWarning::AboveMaximum => Text::SourceAboveMaximum,
                        SourceWarning::BelowMinimum => Text::SourceBelowMinimum,
                    }),
                );
            }
            ui.add_space(16.0);
            let editable = state.source.pending.is_none()
                && !state.calibration.busy()
                && state.sweep != SweepState::Stopping
                && state.connection != ConnectionState::Disconnecting;
            ui.add_enabled_ui(editable, |ui| {
                let draft = state.source.config.draft_mut(kind);
                ui.columns(2, |columns| {
                    columns[0].heading(language.text(Text::SourceFrequency));
                    columns[0].scope(|ui| {
                        ui.style_mut().override_font_id = Some(egui::FontId::monospace(32.0));
                        let before = draft.frequency_hz;
                        let response = ui.add(
                            egui::DragValue::new(&mut draft.frequency_hz)
                                .range(0..=kind.max_frequency_hz())
                                .clamp_existing_to_range(false)
                                .speed(1)
                                .custom_formatter(|value, _| format!("{:.6}", value / 1_000_000.0))
                                .custom_parser(|text| parse_scaled_hz(text, 1_000_000.0))
                                .suffix(" MHz"),
                        );
                        let delta = digit_wheel_delta(
                            ui,
                            &response,
                            &format!("{:.6}", before as f64 / 1_000_000.0),
                            " MHz",
                        );
                        if delta != 0 {
                            draft.frequency_hz = draft
                                .frequency_hz
                                .saturating_add_signed(delta)
                                .min(kind.max_frequency_hz());
                        }
                    });
                    columns[1].heading(language.text(Text::SourceAmplitude));
                    columns[1].scope(|ui| {
                        ui.style_mut().override_font_id = Some(egui::FontId::monospace(32.0));
                        if draft.port == "afout" {
                            let before = draft.amplitude_mv;
                            let response = ui.add(
                                egui::DragValue::new(&mut draft.amplitude_mv)
                                    .range(0..=AFOUT_MAX_MV)
                                    .clamp_existing_to_range(false)
                                    .custom_formatter(|value, _| format!("{value:.0}"))
                                    .suffix(" mV VPP"),
                            );
                            let delta =
                                digit_wheel_delta(ui, &response, &before.to_string(), " mV VPP");
                            if delta != 0 {
                                draft.amplitude_mv = (i64::from(draft.amplitude_mv) + delta)
                                    .clamp(0, i64::from(AFOUT_MAX_MV))
                                    as u32;
                            }
                        } else {
                            let (min, max) = kind.amplitude_dbm_range();
                            let before = draft.amplitude_dbm;
                            let response = ui.add(
                                egui::DragValue::new(&mut draft.amplitude_dbm)
                                    .range(min..=max)
                                    .clamp_existing_to_range(false)
                                    .custom_formatter(|value, _| format!("{value:.0}"))
                                    .suffix(" dBm"),
                            );
                            let delta =
                                digit_wheel_delta(ui, &response, &before.to_string(), " dBm");
                            if delta != 0 {
                                draft.amplitude_dbm = (i64::from(draft.amplitude_dbm) + delta)
                                    .clamp(i64::from(min), i64::from(max))
                                    as i32;
                            }
                        }
                    });
                    columns[1].add_space(12.0);
                    columns[1].label(language.text(Text::SourcePort));
                    columns[1].horizontal(|ui| {
                        for &port in kind.ports() {
                            ui.selectable_value(
                                &mut draft.port,
                                port.wire_name().into(),
                                language.text(match port {
                                    SourcePort::Port1 => Text::SourcePort1,
                                    SourcePort::Port2 => Text::SourcePort2,
                                    SourcePort::AfOut => Text::SourceAfOut,
                                }),
                            );
                        }
                    });
                });
                ui.add_space(24.0);
                ui.separator();
                ui.add_space(12.0);
                modulation_fields(ui, draft, kind, language);
            });
            ui.add_space(24.0);
            let params = state.source.config.draft(kind).params(kind);
            if let Err(error) = &params {
                let message = error.to_string();
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&message).color(ui.visuals().error_fg_color),
                    )
                    .truncate(),
                )
                .on_hover_text(message);
            }
            let requested = matches!(state.source.report.state, SourceOutputState::Requested(_));
            let can_start = state.connection == ConnectionState::Connected
                && editable
                && state.sweep == SweepState::Idle
                && !matches!(state.source.report.state, SourceOutputState::Unknown)
                && params.as_ref().is_ok_and(|params| {
                    !requested || state.source.requested.as_ref() != Some(params)
                });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        can_start,
                        egui::Button::new(language.text(if requested {
                            Text::Apply
                        } else {
                            Text::SourceStart
                        }))
                        .min_size(egui::vec2(140.0, 36.0)),
                    )
                    .clicked()
                    && let Ok(params) = params
                {
                    state.send(WorkerCommand::StartSource(params));
                }
                if ui
                    .add_enabled(
                        state.connection == ConnectionState::Connected
                            && state.source.busy()
                            && state.source.pending != Some(SourcePending::Stop),
                        egui::Button::new(language.text(Text::StopSweep))
                            .min_size(egui::vec2(100.0, 36.0)),
                    )
                    .clicked()
                {
                    state.send(WorkerCommand::StopSource);
                }
                ui.small(language.text(Text::SourceApplyHelp));
            });
        });
}

#[derive(Clone, Copy, Default)]
struct WheelRemainder {
    step: i64,
    value: f64,
}

fn digit_step(number: &str, index: usize) -> Option<i64> {
    number
        .as_bytes()
        .get(index)
        .filter(|byte| byte.is_ascii_digit())?;
    let right = number[index + 1..]
        .bytes()
        .filter(u8::is_ascii_digit)
        .count();
    10_i64.checked_pow(u32::try_from(right).ok()?)
}

/// Consume original wheel events once. Smoothing frames cannot replay an edit.
fn digit_wheel_delta(ui: &egui::Ui, response: &egui::Response, number: &str, suffix: &str) -> i64 {
    let id = response.id.with("digit_wheel");
    if !response.hovered() || !response.enabled() || response.has_focus() || response.dragged() {
        ui.data_mut(|data| data.remove::<WheelRemainder>(id));
        return 0;
    }
    let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) else {
        return 0;
    };
    let font = ui
        .style()
        .override_font_id
        .clone()
        .unwrap_or_else(|| ui.style().drag_value_text_style.resolve(ui.style()));
    let galley =
        ui.painter()
            .layout_no_wrap(format!("{number}{suffix}"), font, ui.visuals().text_color());
    let inner = response.rect.shrink2(ui.spacing().button_padding);
    let origin = egui::Align2([ui.layout().horizontal_align(), ui.layout().vertical_align()])
        .align_size_within_rect(galley.size(), inner)
        .min;
    let Some(row) = galley.rows.first() else {
        return 0;
    };
    let Some((index, glyph)) = row.glyphs.iter().enumerate().find(|(_, glyph)| {
        let left = origin.x + glyph.pos.x;
        pointer.x >= left && pointer.x < left + glyph.advance_width
    }) else {
        return 0;
    };
    let Some(step) = digit_step(number, index) else {
        ui.data_mut(|data| data.remove::<WheelRemainder>(id));
        return 0;
    };
    let left = origin.x + glyph.pos.x;
    let bottom = (origin.y + galley.size().y).min(response.rect.bottom() - 1.0);
    ui.painter().line_segment(
        [
            egui::pos2(left, bottom),
            egui::pos2(left + glyph.advance_width, bottom),
        ],
        egui::Stroke::new(1.0, ui.visuals().hyperlink_color),
    );
    response
        .clone()
        .on_hover_text(crate::i18n::language(ui.ctx()).text(Text::SourceWheelHelp));
    let scroll = ui.input_mut(|input| {
        let mut scroll = 0.0;
        let mut consumed = false;
        input.events.retain(|event| {
            if let egui::Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } = event
                && modifiers.is_none()
                && delta.y.is_finite()
                && delta.y != 0.0
                && delta.x == 0.0
            {
                consumed = true;
                scroll += f64::from(delta.y)
                    / if *unit == egui::MouseWheelUnit::Point {
                        40.0
                    } else {
                        1.0
                    };
                false
            } else {
                true
            }
        });
        if consumed {
            input.smooth_scroll_delta.y = 0.0;
        }
        scroll
    });
    if scroll == 0.0 {
        return 0;
    }
    let count = ui.data_mut(|data| {
        let remainder = data.get_temp_mut_or_default::<WheelRemainder>(id);
        if remainder.step != step {
            *remainder = WheelRemainder { step, value: 0.0 };
        }
        remainder.value = (remainder.value + scroll).clamp(-100.0, 100.0);
        let count = remainder.value.trunc() as i64;
        remainder.value -= count as f64;
        count
    });
    step.saturating_mul(count)
}

fn modulation_fields(
    ui: &mut egui::Ui,
    draft: &mut SourceDraft,
    kind: SourceKind,
    language: Language,
) {
    ui.columns(3, |columns| {
        columns[0].heading(language.text(Text::SourceModulation));
        columns[0].horizontal_wrapped(|ui| {
            for (value, label) in [("off", "OFF"), ("ask", "ASK"), ("fm", "FM"), ("pm", "PM")] {
                if value != "pm" || kind == SourceKind::Af {
                    ui.selectable_value(&mut draft.modulation, value.into(), label);
                }
            }
        });
        columns[1].heading(language.text(Text::SourceModFrequency));
        let modulation = draft.modulation().unwrap_or(Modulation::Off);
        let (min, max) = modulation.frequency_range(kind);
        columns[1].add_enabled_ui(modulation != Modulation::Off, |ui| {
            ui.add(
                egui::DragValue::new(&mut draft.modulation_frequency_hz)
                    .range(min..=max.max(min))
                    .suffix(" Hz")
                    .clamp_existing_to_range(false),
            );
        });
        columns[2].heading(language.text(Text::SourceDepth));
        match modulation {
            Modulation::Off => {
                columns[2].label(language.text(Text::SourceNoModulation));
            }
            Modulation::Ask { .. } => {
                if kind == SourceKind::Rf {
                    columns[2].horizontal_wrapped(|ui| {
                        for &depth in RF_ASK_DEPTHS {
                            ui.selectable_value(
                                &mut draft.ask_depth_percent,
                                depth,
                                format!("{depth}%"),
                            );
                        }
                    });
                } else {
                    columns[2].add(
                        egui::DragValue::new(&mut draft.ask_depth_percent)
                            .range(0..=100)
                            .clamp_existing_to_range(false)
                            .suffix(" %"),
                    );
                }
            }
            Modulation::Fm { .. } => {
                let max = if kind == SourceKind::Rf {
                    rf_fm_deviation_max_hz(draft.frequency_hz)
                } else {
                    AF_FM_MAX_HZ
                };
                columns[2].add(
                    egui::DragValue::new(&mut draft.fm_deviation_hz)
                        .range(0..=max)
                        .clamp_existing_to_range(false)
                        .custom_formatter(|value, _| scaled_readout(value, 1_000.0, 3))
                        .custom_parser(|text| parse_scaled_hz(text, 1_000.0))
                        .suffix(" kHz"),
                );
            }
            Modulation::Pm { .. } => {
                columns[2].add(
                    egui::DragValue::new(&mut draft.pm_phase_deg)
                        .range(-180..=180)
                        .clamp_existing_to_range(false)
                        .suffix(" deg"),
                );
            }
        }
    });
}

fn scaled_readout(value: f64, multiplier: f64, precision: usize) -> String {
    format!("{:.precision$}", value / multiplier)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn parse_scaled_hz(text: &str, multiplier: f64) -> Option<f64> {
    let value = text.trim().parse::<f64>().ok()? * multiplier;
    (value.is_finite()
        && (0.0..=7_000_000_000.0).contains(&value)
        && (value - value.round()).abs() < 1e-6)
        .then(|| value.round())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_frame(
        ctx: &egui::Context,
        state: &mut AppState,
        events: Vec<egui::Event>,
        time: &mut f64,
    ) -> Vec<egui::epaint::ClippedShape> {
        *time += 1.0 / 60.0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                time: Some(*time),
                events,
                ..Default::default()
            },
            |ui| show(ui, state),
        );
        let shapes = std::mem::take(&mut output.shapes);
        output.drop_without_applying_deltas();
        shapes
    }

    fn painted_digit(
        shapes: &[egui::epaint::ClippedShape],
        number: &str,
        index: usize,
    ) -> egui::Pos2 {
        shapes
            .iter()
            .find_map(|shape| {
                let egui::Shape::Text(text) = &shape.shape else {
                    return None;
                };
                if text.galley.job.text != number {
                    return None;
                }
                let row = &text.galley.rows[0];
                let glyph = &row.glyphs[index];
                Some(
                    text.pos
                        + row.pos.to_vec2()
                        + egui::vec2(glyph.pos.x + glyph.advance_width * 0.5, row.size.y * 0.5),
                )
            })
            .unwrap_or_else(|| panic!("missing painted number {number}"))
    }

    fn wheel(
        pointer: egui::Pos2,
        delta: f32,
        unit: egui::MouseWheelUnit,
        modifiers: egui::Modifiers,
    ) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pointer),
            egui::Event::MouseWheel {
                unit,
                delta: egui::vec2(0.0, delta),
                phase: egui::TouchPhase::Move,
                modifiers,
            },
        ]
    }

    #[test]
    fn painted_frequency_digits_edit_only_drafts_and_accumulate_original_trackpad_events() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = AppState {
            function: InstrumentFunction::RfSource,
            connection: ConnectionState::Connected,
            cmd_tx: Some(tx),
            ..Default::default()
        };
        state.source.config.rf.frequency_hz = 1_234_567;
        state.source.report.state = SourceOutputState::Requested(SourceKind::Rf);
        let mut time = 0.0;
        let mut shapes = Vec::new();
        for _ in 0..3 {
            shapes = source_frame(&ctx, &mut state, Vec::new(), &mut time);
        }
        let pointer = painted_digit(&shapes, "1.234567", 4);
        source_frame(
            &ctx,
            &mut state,
            wheel(pointer, 1.0, egui::MouseWheelUnit::Line, Default::default()),
            &mut time,
        );
        assert_eq!(state.source.config.rf.frequency_hz, 1_235_567);
        assert!(rx.try_recv().is_err());
        shapes = source_frame(&ctx, &mut state, Vec::new(), &mut time);
        let pointer = painted_digit(&shapes, "1.235567", 7);
        for _ in 0..3 {
            source_frame(
                &ctx,
                &mut state,
                wheel(
                    pointer,
                    10.0,
                    egui::MouseWheelUnit::Point,
                    Default::default(),
                ),
                &mut time,
            );
        }
        assert_eq!(state.source.config.rf.frequency_hz, 1_235_567);
        source_frame(
            &ctx,
            &mut state,
            wheel(
                pointer,
                10.0,
                egui::MouseWheelUnit::Point,
                Default::default(),
            ),
            &mut time,
        );
        assert_eq!(state.source.config.rf.frequency_hz, 1_235_568);
        for _ in 0..10 {
            source_frame(&ctx, &mut state, Vec::new(), &mut time);
        }
        assert_eq!(state.source.config.rf.frequency_hz, 1_235_568);
        source_frame(
            &ctx,
            &mut state,
            wheel(
                pointer,
                1.0,
                egui::MouseWheelUnit::Line,
                egui::Modifiers::CTRL,
            ),
            &mut time,
        );
        assert_eq!(state.source.config.rf.frequency_hz, 1_235_568);
        assert!(ctx.input(|input| input.events.iter().any(
            |event| matches!(event, egui::Event::MouseWheel { modifiers, .. } if modifiers.ctrl)
        )));
        assert!(rx.try_recv().is_err());
        click_control(
            &ctx,
            &mut state,
            Language::English.text(Text::Apply),
            &mut time,
        );
        assert!(
            matches!(rx.try_recv().unwrap().command, WorkerCommand::StartSource(params) if params.frequency_hz == 1_235_568)
        );
    }

    #[test]
    fn painted_amplitude_digits_and_carrier_limits_preserve_signed_units() {
        for (initial, index, delta, expected) in [
            (-23, 1, 1.0, -13),
            (-23, 2, -1.0, -24),
            (9, 0, 1.0, 10),
            (10, 0, 1.0, 10),
            (-30, 2, -1.0, -30),
        ] {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let mut state = AppState {
                function: InstrumentFunction::RfSource,
                ..Default::default()
            };
            state.source.config.rf.amplitude_dbm = initial;
            let mut time = 0.0;
            let mut shapes = Vec::new();
            for _ in 0..3 {
                shapes = source_frame(&ctx, &mut state, Vec::new(), &mut time);
            }
            let pointer = painted_digit(&shapes, &initial.to_string(), index);
            source_frame(
                &ctx,
                &mut state,
                wheel(
                    pointer,
                    delta,
                    egui::MouseWheelUnit::Line,
                    Default::default(),
                ),
                &mut time,
            );
            assert_eq!(state.source.config.rf.amplitude_dbm, expected);
        }
        for (hz, delta) in [(0, -1.0), (7_000_000_000, 1.0)] {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let mut state = AppState {
                function: InstrumentFunction::RfSource,
                ..Default::default()
            };
            state.source.config.rf.frequency_hz = hz;
            let mut time = 0.0;
            let mut shapes = Vec::new();
            for _ in 0..3 {
                shapes = source_frame(&ctx, &mut state, Vec::new(), &mut time);
            }
            let pointer = painted_digit(&shapes, &format!("{:.6}", hz as f64 / 1_000_000.0), 0);
            source_frame(
                &ctx,
                &mut state,
                wheel(
                    pointer,
                    delta,
                    egui::MouseWheelUnit::Line,
                    Default::default(),
                ),
                &mut time,
            );
            assert_eq!(state.source.config.rf.frequency_hz, hz);
        }
    }

    #[test]
    fn scaled_frequency_controls_preserve_whole_hz() {
        for (multiplier, precision) in [(1_000.0, 3), (1_000_000.0, 6)] {
            for hz in [0, 1, 999, 1_234, 1_000_001, 7_000_000_000_u64] {
                let value = hz as f64;
                assert_eq!(
                    parse_scaled_hz(&scaled_readout(value, multiplier, precision), multiplier),
                    Some(value)
                );
            }
            for invalid in ["NaN", "inf", "-1", "1e100", "0.00000001"] {
                assert!(parse_scaled_hz(invalid, multiplier).is_none());
            }
        }
    }

    fn click_control(ctx: &egui::Context, state: &mut AppState, caption: &str, time: &mut f64) {
        let mut pointer = egui::Pos2::ZERO;
        for frame in 0..6 {
            *time += 1.0 / 30.0;
            let events = if frame == 3 || frame == 4 {
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed: frame == 3,
                        modifiers: Default::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    time: Some(*time),
                    events,
                    ..Default::default()
                },
                |ui| show(ui, state),
            );
            let shapes = std::mem::take(&mut output.shapes);
            output.drop_without_applying_deltas();
            if frame == 2 {
                pointer = shapes
                    .iter()
                    .find_map(|shape| {
                        if let egui::Shape::Text(text) = &shape.shape
                            && text.galley.job.text == caption
                        {
                            Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                        } else {
                            None
                        }
                    })
                    .expect("source control visible");
            }
        }
    }

    #[test]
    fn source_edits_wait_for_explicit_apply_and_stop_stays_available() {
        for language in Language::ALL {
            let ctx = egui::Context::default();
            crate::theme::setup(&ctx);
            let (tx, rx) = std::sync::mpsc::channel();
            let mut state = AppState {
                language,
                function: InstrumentFunction::RfSource,
                connection: ConnectionState::Connected,
                cmd_tx: Some(tx),
                ..Default::default()
            };
            let mut time = 0.0;
            click_control(
                &ctx,
                &mut state,
                language.text(Text::SourceStart),
                &mut time,
            );
            let first = rx.try_recv().unwrap();
            let WorkerCommand::StartSource(params) = first.command else {
                panic!("source start expected")
            };
            assert_eq!(
                params,
                state.source.config.rf.params(SourceKind::Rf).unwrap()
            );
            assert_eq!(state.source.pending, Some(SourcePending::Start));
            assert!(!state.any_running());
            assert!(state.active_plan.is_none());
            state.source.accept(
                SourceReport {
                    state: SourceOutputState::Requested(SourceKind::Rf),
                    warning: None,
                },
                state.function,
            );
            state.source.config.rf.frequency_hz += 1;
            assert!(rx.try_recv().is_err());
            click_control(&ctx, &mut state, language.text(Text::Apply), &mut time);
            let updated = rx.try_recv().unwrap();
            let WorkerCommand::StartSource(updated_params) = updated.command else {
                panic!("source apply expected")
            };
            assert_eq!(updated_params.frequency_hz, params.frequency_hz + 1);
            assert!(updated.request_id > first.request_id);
            click_control(&ctx, &mut state, language.text(Text::StopSweep), &mut time);
            assert!(matches!(
                rx.try_recv().unwrap().command,
                WorkerCommand::StopSource
            ));
            assert_eq!(state.source.pending, Some(SourcePending::Stop));
        }
    }

    #[test]
    fn source_drafts_preserve_units_and_reject_unsupported_values() {
        let mut config = SourceConfig::default();
        let rf = config.rf.params(SourceKind::Rf).unwrap();
        assert_eq!(rf.amplitude, SourceAmplitude::Dbm(-30));
        let af = config.af.params(SourceKind::Af).unwrap();
        assert_eq!(af.amplitude, SourceAmplitude::Millivolts(100));
        config.af.amplitude_dbm = -70;
        config.af.amplitude_mv = 1_234;
        config.af.port = "port2".into();
        assert_eq!(
            config.af.params(SourceKind::Af).unwrap().amplitude,
            SourceAmplitude::Dbm(-70)
        );
        config.af.port = "afout".into();
        assert_eq!(
            config.af.params(SourceKind::Af).unwrap().amplitude,
            SourceAmplitude::Millivolts(1_234)
        );
        config.rf.modulation = "ask".into();
        config.rf.ask_depth_percent = 25;
        assert!(config.rf.params(SourceKind::Rf).is_err());
        config.rf.ask_depth_percent = 30;
        assert!(config.rf.params(SourceKind::Rf).is_ok());
        config.rf.modulation = "pm".into();
        assert!(config.rf.params(SourceKind::Rf).is_err());
        config.af.modulation = "pm".into();
        config.af.pm_phase_deg = -180;
        assert!(config.af.params(SourceKind::Af).is_ok());
        config.af.modulation_frequency_hz = 15;
        assert!(config.af.params(SourceKind::Af).is_err());
    }

    #[test]
    fn lost_request_stays_unknown_across_reconnect_and_stop_rejects_late_start() {
        let params = SourceConfig::default().rf.params(SourceKind::Rf).unwrap();
        let mut ui = SourceUi {
            pending: Some(SourcePending::Start),
            requested: Some(params),
            ..Default::default()
        };
        ui.lost();
        assert_eq!(ui.report.state, SourceOutputState::Unknown);
        ui.connected();
        assert_eq!(ui.report.state, SourceOutputState::Unknown);
        assert!(ui.busy());
        ui.pending = Some(SourcePending::Stop);
        ui.requested = Some(params);
        ui.accept(
            SourceReport {
                state: SourceOutputState::Requested(SourceKind::Rf),
                warning: None,
            },
            InstrumentFunction::RfSource,
        );
        assert_eq!(ui.pending, Some(SourcePending::Stop));
        assert_eq!(ui.report.state, SourceOutputState::Unknown);
        ui.accept(
            SourceReport {
                state: SourceOutputState::StopSent,
                warning: None,
            },
            InstrumentFunction::RfSource,
        );
        assert!(!ui.busy());
        ui.lost();
        assert_eq!(ui.report.state, SourceOutputState::StopSent);
    }

    #[test]
    fn source_panels_fit_both_languages_themes_and_window_sizes() {
        for size in [egui::vec2(960.0, 600.0), egui::vec2(1280.0, 850.0)] {
            for language in Language::ALL {
                for mode in [
                    crate::theme::ThemeMode::Light,
                    crate::theme::ThemeMode::Dark,
                ] {
                    for function in [InstrumentFunction::RfSource, InstrumentFunction::AfSource] {
                        let ctx = egui::Context::default();
                        crate::theme::setup(&ctx);
                        crate::theme::apply(&ctx, mode);
                        let mut state = AppState {
                            function,
                            language,
                            connection: ConnectionState::Connected,
                            ..Default::default()
                        };
                        let kind = function.kind().unwrap();
                        let draft = state.source.config.draft_mut(kind);
                        draft.frequency_hz = kind.max_frequency_hz();
                        draft.amplitude_mv = AFOUT_MAX_MV;
                        draft.modulation = if kind == SourceKind::Af { "pm" } else { "fm" }.into();
                        draft.pm_phase_deg = -180;
                        draft.fm_deviation_hz = rf_fm_deviation_max_hz(draft.frequency_hz);
                        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        for _ in 0..3 {
                            let mut output = ctx.run_ui(
                                egui::RawInput {
                                    screen_rect: Some(screen),
                                    ..Default::default()
                                },
                                |ui| show(ui, &mut state),
                            );
                            let shapes = std::mem::take(&mut output.shapes);
                            output.drop_without_applying_deltas();
                            for shape in shapes {
                                if let egui::Shape::Text(text) = shape.shape {
                                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                    assert!(
                                        screen.contains_rect(bounds),
                                        "{language:?} {function:?}: {} {bounds:?}",
                                        text.galley.job.text
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
