// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Spectrum trace plot, custom-drawn with `egui::Painter`.
//!
//! Deliberately not egui_plot: the reference interface uses per-axis
//! strips and cursor-anchored zoom that do not fit owned axes
//! (the reference UI analysis section 6).

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind};
use kcsdi_core::data::SweepData;

use crate::theme::TRACE_COLORS;

/// Visible data range of the plot (frequency x level).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotView {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

impl PlotView {
    pub fn new(x_min: f64, x_max: f64, y_min: f64, y_max: f64) -> Self {
        Self {
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }

    /// Reset the view to the given range (e.g. after parameters changed).
    pub fn reset(&mut self, x_min: f64, x_max: f64, y_min: f64, y_max: f64) {
        *self = Self::new(x_min, x_max, y_min, y_max);
    }
}

/// Grid density target; the reference chart uses 10 segments per axis
/// (reference UI analysis section 2.4).
const TARGET_DIVISIONS: usize = 10;

const MARGIN_LEFT: f32 = 48.0;
const MARGIN_RIGHT: f32 = 8.0;
const MARGIN_TOP: f32 = 8.0;
const MARGIN_BOTTOM: f32 = 20.0;
const LABEL_FONT_SIZE: f32 = 11.0;
const MIN_SPAN: f64 = 1e-9;

// Chart chrome colors from reference UI analysis section 4.3 (dark theme).
const BG_COLOR: Color32 = Color32::from_rgb(0x18, 0x18, 0x18);
const GRID_COLOR: Color32 = Color32::from_rgb(0x42, 0x42, 0x42);
const TEXT_COLOR: Color32 = Color32::from_rgb(0xd5, 0xd5, 0xd5);
const CURSOR_COLOR: Color32 = Color32::from_rgb(0x90, 0x90, 0x90);

/// Pick a 1-2-5 step so `span / step` lands near `target_divs`.
fn nice_step(span: f64, target_divs: usize) -> f64 {
    if !span.is_finite() || span <= 0.0 {
        return 1.0;
    }
    let raw = span / target_divs.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let nice = if norm < 1.5 {
        1.0
    } else if norm < 3.0 {
        2.0
    } else if norm < 7.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

/// Like [`nice_step`], but rounded up to an integer step of at least 1,
/// for axes whose labels must stay whole numbers (dBm).
fn nice_int_step(span: f64, target_divs: usize) -> i64 {
    nice_step(span, target_divs).ceil().max(1.0) as i64
}

/// SI unit and scale for a frequency magnitude: Hz below 1 kHz, then
/// kHz, MHz, GHz.
fn si_unit(hz: f64) -> (&'static str, f64) {
    let abs = hz.abs();
    if abs >= 1e9 {
        ("GHz", 1e9)
    } else if abs >= 1e6 {
        ("MHz", 1e6)
    } else if abs >= 1e3 {
        ("kHz", 1e3)
    } else {
        ("Hz", 1.0)
    }
}

/// Decimals needed to print `step` without losing it. `step` is a 1-2-5
/// multiple of a power of ten, so a short scan suffices.
fn decimals(step: f64) -> usize {
    for d in 0..=6usize {
        if (step * 10f64.powi(d as i32)).fract().abs() < 1e-9 {
            return d;
        }
    }
    6
}

/// First tick value at or above `min` on a grid of `step`.
fn first_tick(min: f64, step: f64) -> f64 {
    (min / step).ceil() * step
}

fn format_freq(hz: f64, unit: &str, scale: f64, decimals: usize) -> String {
    format!("{:.*} {}", decimals, hz / scale, unit)
}

/// Draw the plot into the available space. Signature is a module
/// contract; do not change it.
pub fn show(ui: &mut egui::Ui, view: &mut PlotView, trace: Option<&SweepData>) {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        Pos2::new(rect.left() + MARGIN_LEFT, rect.top() + MARGIN_TOP),
        Pos2::new(rect.right() - MARGIN_RIGHT, rect.bottom() - MARGIN_BOTTOM),
    );

    handle_input(ui, view, trace, plot_rect, &response);

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, BG_COLOR);
    if plot_rect.width() < 2.0 || plot_rect.height() < 2.0 {
        return;
    }

    draw_grid(painter, view, plot_rect);

    match trace {
        Some(data) if !data.points.is_empty() => {
            let clipped = painter.with_clip_rect(plot_rect);
            draw_trace(&clipped, view, data);
        }
        _ => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No data",
                FontId::monospace(14.0),
                TEXT_COLOR,
            );
        }
    }

    draw_cursor(painter, view, plot_rect, &response);
}

/// Wheel zoom (10% per step, anchored at the cursor; Shift selects the Y
/// axis), drag pan, and double-click reset, per reference UI analysis section 5.
fn handle_input(
    ui: &egui::Ui,
    view: &mut PlotView,
    trace: Option<&SweepData>,
    plot_rect: Rect,
    response: &egui::Response,
) {
    let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
    if scroll != 0.0
        && response.hovered()
        && let Some(pos) = response.hover_pos()
        && plot_rect.contains(pos)
    {
        let factor = if scroll > 0.0 { 0.9 } else { 1.0 / 0.9 };
        let shift = ui.ctx().input(|i| i.modifiers.shift);
        if shift {
            let anchor = view.y_max
                - ((pos.y - plot_rect.top()) / plot_rect.height()) as f64
                    * (view.y_max - view.y_min);
            zoom_axis(&mut view.y_min, &mut view.y_max, anchor, factor);
        } else {
            let anchor = view.x_min
                + ((pos.x - plot_rect.left()) / plot_rect.width()) as f64
                    * (view.x_max - view.x_min);
            zoom_axis(&mut view.x_min, &mut view.x_max, anchor, factor);
        }
    }

    if response.dragged() {
        let d = response.drag_delta();
        if plot_rect.width() > 0.0 && plot_rect.height() > 0.0 {
            let dx = -(d.x as f64 / plot_rect.width() as f64) * (view.x_max - view.x_min);
            view.x_min += dx;
            view.x_max += dx;
            let dy = (d.y as f64 / plot_rect.height() as f64) * (view.y_max - view.y_min);
            view.y_min += dy;
            view.y_max += dy;
        }
    }

    if response.double_clicked()
        && let Some((x_min, x_max, y_min, y_max)) = data_range(trace)
    {
        view.reset(x_min, x_max, y_min, y_max);
    }
}

/// Zoom one axis around `anchor`, refusing to collapse below MIN_SPAN.
fn zoom_axis(min: &mut f64, max: &mut f64, anchor: f64, factor: f64) {
    if (*max - *min).abs() * factor < MIN_SPAN {
        return;
    }
    *min = anchor - (anchor - *min) * factor;
    *max = anchor + (*max - anchor) * factor;
}

/// Full data extent of the trace: frequency range and values[0] range
/// with 5% headroom, for the double-click reset.
fn data_range(trace: Option<&SweepData>) -> Option<(f64, f64, f64, f64)> {
    let points = &trace?.points;
    let mut range: Option<(f64, f64, f64, f64)> = None;
    for p in points {
        let Some(&v) = p.values.first() else {
            continue;
        };
        if !p.freq_hz.is_finite() || !v.is_finite() {
            continue;
        }
        range = Some(match range {
            None => (p.freq_hz, p.freq_hz, v, v),
            Some((x0, x1, y0, y1)) => (x0.min(p.freq_hz), x1.max(p.freq_hz), y0.min(v), y1.max(v)),
        });
    }
    range.map(|(x0, x1, y0, y1)| {
        let pad = ((y1 - y0) * 0.05).max(1.0);
        (x0, x1, y0 - pad, y1 + pad)
    })
}

fn draw_grid(painter: &egui::Painter, view: &PlotView, plot_rect: Rect) {
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let x_span = view.x_max - view.x_min;
    let y_span = view.y_max - view.y_min;

    // X axis: frequency with an SI prefix chosen from the visible range.
    let (unit, scale) = si_unit(view.x_min.abs().max(view.x_max.abs()));
    let x_step = nice_step(x_span, TARGET_DIVISIONS);
    let x_dec = decimals(x_step / scale);
    let mut x = first_tick(view.x_min, x_step);
    while x <= view.x_max + x_step * 1e-6 {
        let sx = plot_rect.left() + ((x - view.x_min) / x_span) as f32 * plot_rect.width();
        painter.line_segment(
            [
                Pos2::new(sx, plot_rect.top()),
                Pos2::new(sx, plot_rect.bottom()),
            ],
            Stroke::new(1.0, GRID_COLOR),
        );
        painter.text(
            Pos2::new(sx, plot_rect.bottom() + 3.0),
            Align2::CENTER_TOP,
            format_freq(x, unit, scale, x_dec),
            font.clone(),
            TEXT_COLOR,
        );
        x += x_step;
    }

    // Y axis: whole-dBm steps so labels stay integers.
    let y_step = nice_int_step(y_span, TARGET_DIVISIONS) as f64;
    let mut y = first_tick(view.y_min, y_step);
    while y <= view.y_max + y_step * 1e-6 {
        let sy = plot_rect.bottom() - ((y - view.y_min) / y_span) as f32 * plot_rect.height();
        painter.line_segment(
            [
                Pos2::new(plot_rect.left(), sy),
                Pos2::new(plot_rect.right(), sy),
            ],
            Stroke::new(1.0, GRID_COLOR),
        );
        painter.text(
            Pos2::new(plot_rect.left() - 4.0, sy),
            Align2::RIGHT_CENTER,
            format!("{}", y.round() as i64),
            font.clone(),
            TEXT_COLOR,
        );
        y += y_step;
    }
    painter.text(
        Pos2::new(plot_rect.left() + 4.0, plot_rect.top() + 2.0),
        Align2::LEFT_TOP,
        "dBm",
        font.clone(),
        TEXT_COLOR,
    );

    painter.rect_stroke(
        plot_rect,
        0.0,
        Stroke::new(1.0, GRID_COLOR),
        StrokeKind::Inside,
    );
}

/// Draw the trace polyline. Only the visible index window (plus one
/// neighbor on each side, for continuity at the edges) is projected to
/// screen coordinates; sweeps are sorted by frequency, so
/// `partition_point` finds the window without scanning everything.
fn draw_trace(painter: &egui::Painter, view: &PlotView, trace: &SweepData) {
    let stroke = Stroke::new(1.5, TRACE_COLORS[0]);
    let points = &trace.points;
    let x_span = (view.x_max - view.x_min).max(MIN_SPAN);
    let y_span = (view.y_max - view.y_min).max(MIN_SPAN);
    let rect = painter.clip_rect();
    let to_screen = |freq: f64, level: f64| {
        Pos2::new(
            rect.left() + ((freq - view.x_min) / x_span) as f32 * rect.width(),
            rect.bottom() - ((level - view.y_min) / y_span) as f32 * rect.height(),
        )
    };

    let start = points
        .partition_point(|p| p.freq_hz < view.x_min)
        .saturating_sub(1);
    let end = (points.partition_point(|p| p.freq_hz <= view.x_max) + 1).min(points.len());

    // Split into runs at non-finite samples so a gap does not smear a
    // line across the plot.
    let mut run: Vec<Pos2> = Vec::new();
    for p in &points[start..end] {
        match p.values.first() {
            Some(&level) if level.is_finite() && p.freq_hz.is_finite() => {
                run.push(to_screen(p.freq_hz, level));
            }
            _ => {
                if run.len() >= 2 {
                    painter.add(Shape::line(std::mem::take(&mut run), stroke));
                } else {
                    run.clear();
                }
            }
        }
    }
    if run.len() >= 2 {
        painter.add(Shape::line(run, stroke));
    }
}

/// Crosshair lines and a (frequency, level) readout of the data
/// coordinates under the mouse.
fn draw_cursor(
    painter: &egui::Painter,
    view: &PlotView,
    plot_rect: Rect,
    response: &egui::Response,
) {
    let Some(pos) = response.hover_pos() else {
        return;
    };
    if !plot_rect.contains(pos) {
        return;
    }
    painter.line_segment(
        [
            Pos2::new(pos.x, plot_rect.top()),
            Pos2::new(pos.x, plot_rect.bottom()),
        ],
        Stroke::new(0.5, CURSOR_COLOR),
    );
    painter.line_segment(
        [
            Pos2::new(plot_rect.left(), pos.y),
            Pos2::new(plot_rect.right(), pos.y),
        ],
        Stroke::new(0.5, CURSOR_COLOR),
    );

    let freq = view.x_min
        + ((pos.x - plot_rect.left()) / plot_rect.width()) as f64 * (view.x_max - view.x_min);
    let level = view.y_max
        - ((pos.y - plot_rect.top()) / plot_rect.height()) as f64 * (view.y_max - view.y_min);
    let (unit, scale) = si_unit(freq.abs());
    painter.text(
        Pos2::new(plot_rect.right() - 4.0, plot_rect.top() + 2.0),
        Align2::RIGHT_TOP,
        format!("{:.3} {}  {:.1} dBm", freq / scale, unit, level),
        FontId::monospace(LABEL_FONT_SIZE),
        TEXT_COLOR,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcsdi_core::protocol::StreamMode;

    #[test]
    fn nice_step_picks_1_2_5() {
        assert_eq!(nice_step(100.0, 10), 10.0);
        assert_eq!(nice_step(90.0, 10), 10.0);
        assert_eq!(nice_step(16.0, 10), 2.0);
        assert_eq!(nice_step(0.3, 10), 0.05);
        assert_eq!(nice_step(1.0, 10), 0.1);
    }

    #[test]
    fn nice_step_handles_bad_input() {
        assert_eq!(nice_step(0.0, 10), 1.0);
        assert_eq!(nice_step(-5.0, 10), 1.0);
        assert_eq!(nice_step(f64::NAN, 10), 1.0);
    }

    #[test]
    fn nice_int_step_stays_a_positive_integer() {
        assert_eq!(nice_int_step(3.0, 10), 1);
        assert_eq!(nice_int_step(100.0, 10), 10);
        assert_eq!(nice_int_step(95.0, 10), 10);
        assert_eq!(nice_int_step(0.4, 10), 1);
    }

    #[test]
    fn si_unit_selects_prefix_by_magnitude() {
        assert_eq!(si_unit(2.4e9), ("GHz", 1e9));
        assert_eq!(si_unit(-433e6), ("MHz", 1e6));
        assert_eq!(si_unit(12e3), ("kHz", 1e3));
        assert_eq!(si_unit(900.0), ("Hz", 1.0));
    }

    #[test]
    fn decimals_match_step() {
        assert_eq!(decimals(10.0), 0);
        assert_eq!(decimals(2.0), 0);
        assert_eq!(decimals(0.5), 1);
        assert_eq!(decimals(0.25), 2);
        assert_eq!(decimals(0.05), 2);
    }

    #[test]
    fn first_tick_lands_on_grid() {
        assert_eq!(first_tick(0.0, 10.0), 0.0);
        assert_eq!(first_tick(3.0, 10.0), 10.0);
        assert_eq!(first_tick(-25.0, 10.0), -20.0);
    }

    #[test]
    fn format_freq_scales_and_decorates() {
        assert_eq!(format_freq(2.4e9, "GHz", 1e9, 2), "2.40 GHz");
        assert_eq!(format_freq(433e6, "MHz", 1e6, 0), "433 MHz");
    }

    #[test]
    fn zoom_axis_keeps_anchor_fixed() {
        let (mut min, mut max) = (0.0, 100.0);
        zoom_axis(&mut min, &mut max, 50.0, 0.9);
        assert!((min - 5.0).abs() < 1e-9);
        assert!((max - 95.0).abs() < 1e-9);
        zoom_axis(&mut min, &mut max, 50.0, 1.0 / 0.9);
        assert!((min - 0.0).abs() < 1e-9);
        assert!((max - 100.0).abs() < 1e-9);
    }

    #[test]
    fn data_range_covers_points_with_headroom() {
        let trace = SweepData {
            mode: StreamMode::Spec,
            format: String::new(),
            points: vec![
                kcsdi_core::data::SweepPoint {
                    freq_hz: 1e6,
                    values: vec![-80.0],
                },
                kcsdi_core::data::SweepPoint {
                    freq_hz: 2e6,
                    values: vec![-20.0],
                },
            ],
        };
        let (x0, x1, y0, y1) = data_range(Some(&trace)).unwrap();
        assert_eq!((x0, x1), (1e6, 2e6));
        assert_eq!(y0, -83.0);
        assert_eq!(y1, -17.0);
        assert!(data_range(None).is_none());
    }

    #[test]
    fn data_range_skips_non_finite_samples() {
        let trace = SweepData {
            mode: StreamMode::Spec,
            format: String::new(),
            points: vec![
                kcsdi_core::data::SweepPoint {
                    freq_hz: 1e6,
                    values: vec![f64::NAN],
                },
                kcsdi_core::data::SweepPoint {
                    freq_hz: 3e6,
                    values: vec![-50.0],
                },
            ],
        };
        let (x0, x1, _, _) = data_range(Some(&trace)).unwrap();
        assert_eq!((x0, x1), (3e6, 3e6));
    }
}
