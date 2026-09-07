// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cartesian trace plot, custom-drawn with `egui::Painter`.
//!
//! Deliberately not egui_plot: the reference interface uses per-axis
//! strips and cursor-anchored zoom that do not fit owned axes
//! (the reference UI analysis section 6). Supports multiple series and
//! an optional logarithmic Y axis (extension over the reference
//! interface).

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind};

/// One drawn trace.
#[derive(Debug, Clone)]
pub struct Series {
    pub color: egui::Color32,
    /// (x, y) data points; non-finite values break the polyline. In
    /// log-Y mode non-positive values also break it.
    pub points: Vec<(f64, f64)>,
}

/// What to draw, prepared by the caller each frame.
pub struct PlotOptions<'a> {
    /// Y axis unit label (e.g. "dBm", "ohm", "deg").
    pub y_label: &'a str,
    /// Logarithmic Y axis (base 10). Non-positive values are skipped.
    pub log_y: bool,
    pub series: Vec<Series>,
}

/// Visible data range of the plot (x x y, in data units).
///
/// In log-Y mode (`PlotOptions::log_y`) `y_min`/`y_max` still hold raw
/// data values, not log10 of them; the widget maps to log space
/// internally for layout, zoom, and pan. Keep both positive there.
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
/// Floor for raw Y values before the log10 mapping, so a view that
/// drifted to zero or below still maps to something finite.
const MIN_POSITIVE: f64 = 1e-300;

// Chart chrome colors from reference UI analysis section 4.3 (dark theme).
const BG_COLOR: Color32 = Color32::from_rgb(0x18, 0x18, 0x18);
const GRID_COLOR: Color32 = Color32::from_rgb(0x42, 0x42, 0x42);
const TEXT_COLOR: Color32 = Color32::from_rgb(0xd5, 0xd5, 0xd5);
const CURSOR_COLOR: Color32 = Color32::from_rgb(0x90, 0x90, 0x90);

/// Map a raw Y value into axis space: identity in linear mode, log10 in
/// log mode (clamped away from zero so the mapping stays finite).
fn to_axis(log_y: bool, v: f64) -> f64 {
    if log_y {
        v.max(MIN_POSITIVE).log10()
    } else {
        v
    }
}

/// Inverse of [`to_axis`].
fn from_axis(log_y: bool, a: f64) -> f64 {
    if log_y { 10f64.powf(a) } else { a }
}

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

/// Label for 10^exp on a log axis: plain up to 100, then k/M/G.
fn format_pow10(exp: i32) -> String {
    const SUFFIX: [&str; 3] = ["k", "M", "G"];
    match exp {
        0 => "1".to_string(),
        1 => "10".to_string(),
        2 => "100".to_string(),
        -1 => "0.1".to_string(),
        -2 => "0.01".to_string(),
        -3 => "0.001".to_string(),
        e if e > 2 && e <= 11 => {
            let suffix = SUFFIX[(e as usize - 3) / 3];
            let mantissa = 10u64.pow((e as u32 - 3) % 3);
            format!("{}{}", mantissa, suffix)
        }
        e => format!("1e{}", e),
    }
}

/// Compact number for tick labels and readouts: fixed point inside a
/// sane magnitude band, scientific outside, trailing zeros trimmed.
fn format_value(v: f64) -> String {
    let a = v.abs();
    if a != 0.0 && !(1e-3..1e6).contains(&a) {
        return format!("{:.3e}", v);
    }
    let s = format!("{:.4}", v);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Draw the plot into the available space. Signature is a module
/// contract; do not change it.
pub fn show(ui: &mut egui::Ui, view: &mut PlotView, opts: &PlotOptions) {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        Pos2::new(rect.left() + MARGIN_LEFT, rect.top() + MARGIN_TOP),
        Pos2::new(rect.right() - MARGIN_RIGHT, rect.bottom() - MARGIN_BOTTOM),
    );

    handle_input(ui, view, opts, plot_rect, &response);

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, BG_COLOR);
    if plot_rect.width() < 2.0 || plot_rect.height() < 2.0 {
        return;
    }

    draw_grid(painter, view, opts, plot_rect);

    if opts.series.iter().any(|s| !s.points.is_empty()) {
        let clipped = painter.with_clip_rect(plot_rect);
        for series in &opts.series {
            draw_series(&clipped, view, opts.log_y, series);
        }
        draw_legend(painter, opts, plot_rect);
    } else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No data",
            FontId::monospace(14.0),
            TEXT_COLOR,
        );
    }

    draw_cursor(painter, view, opts, plot_rect, &response);
}

/// Wheel zoom (10% per step, anchored at the cursor; Shift selects the Y
/// axis), drag pan, and double-click reset, per reference UI analysis section 5.
/// Y math happens in axis space so log mode zooms/pans by decades.
fn handle_input(
    ui: &egui::Ui,
    view: &mut PlotView,
    opts: &PlotOptions,
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
            let (mut a_min, mut a_max) = (
                to_axis(opts.log_y, view.y_min),
                to_axis(opts.log_y, view.y_max),
            );
            let anchor =
                a_max - ((pos.y - plot_rect.top()) / plot_rect.height()) as f64 * (a_max - a_min);
            zoom_axis(&mut a_min, &mut a_max, anchor, factor);
            view.y_min = from_axis(opts.log_y, a_min);
            view.y_max = from_axis(opts.log_y, a_max);
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
            let (mut a_min, mut a_max) = (
                to_axis(opts.log_y, view.y_min),
                to_axis(opts.log_y, view.y_max),
            );
            let dy = (d.y as f64 / plot_rect.height() as f64) * (a_max - a_min);
            a_min += dy;
            a_max += dy;
            view.y_min = from_axis(opts.log_y, a_min);
            view.y_max = from_axis(opts.log_y, a_max);
        }
    }

    if response.double_clicked()
        && let Some((x_min, x_max, y_min, y_max)) = data_range(opts)
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

/// Full data extent over all series, with 5% Y headroom (5% of the
/// decade span in log mode), for the double-click reset. In log mode
/// only positive Y values count.
fn data_range(opts: &PlotOptions) -> Option<(f64, f64, f64, f64)> {
    let mut range: Option<(f64, f64, f64, f64)> = None;
    for series in &opts.series {
        for &(x, y) in &series.points {
            if !x.is_finite() || !y.is_finite() || (opts.log_y && y <= 0.0) {
                continue;
            }
            range = Some(match range {
                None => (x, x, y, y),
                Some((x0, x1, y0, y1)) => (x0.min(x), x1.max(x), y0.min(y), y1.max(y)),
            });
        }
    }
    range.map(|(x0, x1, y0, y1)| {
        if opts.log_y {
            let (a0, a1) = (y0.log10(), y1.log10());
            let pad = ((a1 - a0) * 0.05).max(0.05);
            (x0, x1, 10f64.powf(a0 - pad), 10f64.powf(a1 + pad))
        } else {
            let pad = ((y1 - y0) * 0.05).max(1.0);
            (x0, x1, y0 - pad, y1 + pad)
        }
    })
}

fn draw_grid(painter: &egui::Painter, view: &PlotView, opts: &PlotOptions, plot_rect: Rect) {
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let x_span = view.x_max - view.x_min;
    let (a_min, a_max) = (
        to_axis(opts.log_y, view.y_min),
        to_axis(opts.log_y, view.y_max),
    );
    let a_span = a_max - a_min;
    let y_to_screen =
        |a: f64| plot_rect.bottom() - ((a - a_min) / a_span) as f32 * plot_rect.height();

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

    // Y axis: powers of ten in log mode, 1-2-5 steps in linear mode.
    if opts.log_y {
        let mut exp = a_min.ceil() as i32;
        while (exp as f64) <= a_max + 1e-9 {
            let sy = y_to_screen(exp as f64);
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
                format_pow10(exp),
                font.clone(),
                TEXT_COLOR,
            );
            exp += 1;
        }
    } else {
        let y_step = nice_step(a_span, TARGET_DIVISIONS);
        let y_dec = decimals(y_step);
        let mut y = first_tick(a_min, y_step);
        while y <= a_max + y_step * 1e-6 {
            let sy = y_to_screen(y);
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
                format!("{:.*}", y_dec, y),
                font.clone(),
                TEXT_COLOR,
            );
            y += y_step;
        }
    }
    painter.text(
        Pos2::new(plot_rect.left() + 4.0, plot_rect.top() + 2.0),
        Align2::LEFT_TOP,
        opts.y_label,
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

/// Draw one series polyline. Runs break at non-finite points and, in log
/// mode, at non-positive Y values, so a gap does not smear a line
/// across the plot.
fn draw_series(painter: &egui::Painter, view: &PlotView, log_y: bool, series: &Series) {
    let stroke = Stroke::new(1.5, series.color);
    let x_span = (view.x_max - view.x_min).max(MIN_SPAN);
    let (a_min, a_max) = (to_axis(log_y, view.y_min), to_axis(log_y, view.y_max));
    let a_span = (a_max - a_min).max(MIN_SPAN);
    let rect = painter.clip_rect();
    let to_screen = |x: f64, y: f64| {
        Pos2::new(
            rect.left() + ((x - view.x_min) / x_span) as f32 * rect.width(),
            rect.bottom() - ((to_axis(log_y, y) - a_min) / a_span) as f32 * rect.height(),
        )
    };

    let mut run: Vec<Pos2> = Vec::new();
    for &(x, y) in &series.points {
        if x.is_finite() && y.is_finite() && (!log_y || y > 0.0) {
            run.push(to_screen(x, y));
        } else if run.len() >= 2 {
            painter.add(Shape::line(std::mem::take(&mut run), stroke));
        } else {
            run.clear();
        }
    }
    if run.len() >= 2 {
        painter.add(Shape::line(run, stroke));
    }
}

/// Series key: the shared y_label sits at the top left already, so this
/// only draws per-series color swatches at the top right when more than
/// one series is on screen (series carry no names).
fn draw_legend(painter: &egui::Painter, opts: &PlotOptions, plot_rect: Rect) {
    if opts.series.len() < 2 {
        return;
    }
    let mut x = plot_rect.right() - 16.0;
    let y = plot_rect.top() + 8.0;
    for series in opts.series.iter().rev() {
        painter.line_segment(
            [Pos2::new(x - 12.0, y), Pos2::new(x, y)],
            Stroke::new(2.0, series.color),
        );
        x -= 20.0;
    }
}

/// Crosshair lines and a (frequency, value) readout of the data
/// coordinates under the mouse. Log mode reports the raw value.
fn draw_cursor(
    painter: &egui::Painter,
    view: &PlotView,
    opts: &PlotOptions,
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
    let (a_min, a_max) = (
        to_axis(opts.log_y, view.y_min),
        to_axis(opts.log_y, view.y_max),
    );
    let a = a_max - ((pos.y - plot_rect.top()) / plot_rect.height()) as f64 * (a_max - a_min);
    let value = from_axis(opts.log_y, a);
    let (unit, scale) = si_unit(freq.abs());
    painter.text(
        Pos2::new(plot_rect.right() - 4.0, plot_rect.bottom() - 4.0),
        Align2::RIGHT_BOTTOM,
        format!(
            "{:.3} {}  {} {}",
            freq / scale,
            unit,
            format_value(value),
            opts.y_label
        ),
        FontId::monospace(LABEL_FONT_SIZE),
        TEXT_COLOR,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn format_pow10_labels_decades() {
        assert_eq!(format_pow10(0), "1");
        assert_eq!(format_pow10(1), "10");
        assert_eq!(format_pow10(2), "100");
        assert_eq!(format_pow10(3), "1k");
        assert_eq!(format_pow10(5), "100k");
        assert_eq!(format_pow10(6), "1M");
        assert_eq!(format_pow10(-2), "0.01");
        assert_eq!(format_pow10(12), "1e12");
    }

    #[test]
    fn format_value_trims_and_switches_to_scientific() {
        assert_eq!(format_value(-50.0), "-50");
        assert_eq!(format_value(2.5), "2.5");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(1.5e7), "1.500e7");
        assert_eq!(format_value(2.0e-4), "2.000e-4");
    }

    #[test]
    fn axis_mapping_roundtrips() {
        assert_eq!(to_axis(false, 42.0), 42.0);
        assert_eq!(from_axis(false, 42.0), 42.0);
        assert!((to_axis(true, 100.0) - 2.0).abs() < 1e-12);
        assert!((from_axis(true, 2.0) - 100.0).abs() < 1e-9);
        assert!(to_axis(true, 0.0).is_finite());
        assert!(to_axis(true, -5.0).is_finite());
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

    fn opts_with_points(points: &[(f64, f64)], log_y: bool) -> PlotOptions<'static> {
        PlotOptions {
            y_label: "dBm",
            log_y,
            series: vec![Series {
                color: Color32::WHITE,
                points: points.to_vec(),
            }],
        }
    }

    #[test]
    fn data_range_covers_points_with_headroom() {
        let opts = opts_with_points(&[(1e6, -80.0), (2e6, -20.0)], false);
        let (x0, x1, y0, y1) = data_range(&opts).unwrap();
        assert_eq!((x0, x1), (1e6, 2e6));
        assert_eq!(y0, -83.0);
        assert_eq!(y1, -17.0);
        assert!(data_range(&opts_with_points(&[], false)).is_none());
    }

    #[test]
    fn data_range_skips_non_finite_samples() {
        let opts = opts_with_points(&[(1e6, f64::NAN), (3e6, -50.0)], false);
        let (x0, x1, _, _) = data_range(&opts).unwrap();
        assert_eq!((x0, x1), (3e6, 3e6));
    }

    #[test]
    fn data_range_log_mode_skips_non_positive_and_pads_decades() {
        let opts = opts_with_points(&[(1e6, -5.0), (2e6, 0.0), (3e6, 10.0), (4e6, 1000.0)], true);
        let (x0, x1, y0, y1) = data_range(&opts).unwrap();
        assert_eq!((x0, x1), (3e6, 4e6));
        assert!((y0.log10() - (1.0 - 0.1)).abs() < 1e-9);
        assert!((y1.log10() - (3.0 + 0.1)).abs() < 1e-9);
        assert!(data_range(&opts_with_points(&[(1e6, 0.0)], true)).is_none());
    }
}
