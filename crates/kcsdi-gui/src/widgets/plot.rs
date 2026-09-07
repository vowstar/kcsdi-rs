// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cartesian trace plot, custom-drawn with `egui::Painter`.
//!
//! Supports multiple series, a linear or base-10 logarithmic frequency
//! axis, and cursor-anchored zoom (reference UI analysis section 6).
//! Y always uses the original signed measurement units.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind};

/// One drawn trace.
#[derive(Debug, Clone)]
pub struct Series<'a> {
    /// Trace name, shown in the legend when several series share a plot.
    pub name: &'a str,
    pub color: Color32,
    /// (frequency in Hz, value) points. Non-finite values break the polyline.
    /// Log X additionally excludes non-positive frequencies, never signed Y.
    pub points: Vec<(f64, f64)>,
}

/// What to draw, prepared by the caller each frame.
pub struct PlotOptions<'a> {
    /// Y axis unit label (e.g. "dBm", "ohm", "deg").
    pub y_label: &'a str,
    pub log_x: bool,
    pub series: Vec<Series<'a>>,
}

/// What the user did to the view during one [`show`] frame. Reports
/// whether the view still tracks the data (auto-fit) or the user took
/// manual control of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLock {
    /// No view interaction happened this frame.
    Unchanged,
    /// Wheel zoom or drag pan: the user owns the view now.
    Locked,
    /// Double-click reset: the view tracks the full data range again.
    Unlocked,
}

impl ViewLock {
    /// Combine two reports from the same frame, keeping the stronger
    /// one: Locked > Unlocked > Unchanged.
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Locked, _) | (_, Self::Locked) => Self::Locked,
            (Self::Unlocked, _) | (_, Self::Unlocked) => Self::Unlocked,
            _ => Self::Unchanged,
        }
    }
}

/// Visible range in original data units. X is always in Hz, including
/// in log mode. Y remains linear for layout, readout, zoom, and pan.
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

/// Shared frequency-axis control for SPEC and S11 cartesian displays.
pub fn log_x_control(ui: &mut egui::Ui, log_x: &mut bool) -> bool {
    ui.checkbox(log_x, "LOG X")
        .on_hover_text(
            "Base-10 frequency axis. Only positive frequencies can be shown. Changes the display, not sweep sampling. Y remains linear.",
        )
        .changed()
}

fn usable_x(log_x: bool, x: f64) -> bool {
    x.is_finite() && (!log_x || x > 0.0)
}

fn x_to_axis(log_x: bool, x: f64) -> f64 {
    if log_x { x.log10() } else { x }
}

fn x_from_axis(log_x: bool, x: f64) -> f64 {
    if log_x { 10f64.powf(x) } else { x }
}

/// One mapping shared by curves, grid, cursor, zoom, and pan.
struct Mapping {
    rect: Rect,
    log_x: bool,
    x_min: f64,
    x_span: f64,
    y_min: f64,
    y_span: f64,
}

impl Mapping {
    fn new(view: &PlotView, log_x: bool, rect: Rect) -> Self {
        let x_min = x_to_axis(log_x, view.x_min);
        Self {
            rect,
            log_x,
            x_min,
            x_span: (x_to_axis(log_x, view.x_max) - x_min).max(MIN_SPAN),
            y_min: view.y_min,
            y_span: (view.y_max - view.y_min).max(MIN_SPAN),
        }
    }

    fn to_screen(&self, x: f64, y: f64) -> Pos2 {
        Pos2::new(
            self.rect.left()
                + ((x_to_axis(self.log_x, x) - self.x_min) / self.x_span) as f32
                    * self.rect.width(),
            self.rect.bottom() - ((y - self.y_min) / self.y_span) as f32 * self.rect.height(),
        )
    }

    fn frequency_at(&self, screen_x: f32) -> f64 {
        x_from_axis(
            self.log_x,
            self.x_min + ((screen_x - self.rect.left()) / self.rect.width()) as f64 * self.x_span,
        )
    }

    fn value_at(&self, screen_y: f32) -> f64 {
        self.y_min + ((self.rect.bottom() - screen_y) / self.rect.height()) as f64 * self.y_span
    }
}

/// Refuse zoom/pan bounds that overflow, underflow, or collapse.
fn set_x_bounds(view: &mut PlotView, log_x: bool, axis_min: f64, axis_max: f64) {
    let min = x_from_axis(log_x, axis_min);
    let max = x_from_axis(log_x, axis_max);
    if usable_x(log_x, min) && usable_x(log_x, max) && min < max {
        view.x_min = min;
        view.x_max = max;
    }
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

fn format_freq(hz: f64) -> String {
    let (unit, scale) = si_unit(hz);
    // Retain Hz resolution even with GHz units when zoomed in closely.
    let value = format!("{:.9}", hz / scale);
    format!(
        "{} {}",
        value.trim_end_matches('0').trim_end_matches('.'),
        unit
    )
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

/// Raw 1-2-5 ticks, also useful when zoomed inside one log decade.
fn linear_ticks(min: f64, max: f64) -> Vec<f64> {
    let step = nice_step(max - min, TARGET_DIVISIONS);
    if !step.is_finite() || step <= 0.0 {
        return Vec::new();
    }
    let first = (min / step).ceil();
    let mut ticks = Vec::new();
    // Bound iteration even when floating-point precision cannot advance a tick.
    for i in 0..=TARGET_DIVISIONS * 2 {
        let index = first + i as f64;
        let value = if index == 0.0 { 0.0 } else { index * step };
        if value > max + step * 1e-6 {
            break;
        }
        if value.is_finite() && ticks.last() != Some(&value) {
            ticks.push(value);
        }
    }
    ticks
}

/// Log ticks follow 1, 2, 5 per decade, with labels in actual Hz units.
fn frequency_ticks(log_x: bool, min: f64, max: f64) -> Vec<f64> {
    if !log_x {
        return linear_ticks(min, max);
    }
    if !usable_x(true, min) || !usable_x(true, max) || min >= max {
        return Vec::new();
    }
    let mut ticks = Vec::new();
    for exponent in (min.log10().floor() as i32)..=(max.log10().ceil() as i32) {
        for multiple in [1.0, 2.0, 5.0] {
            let frequency = multiple * 10f64.powi(exponent);
            if frequency >= min && frequency <= max && frequency.is_finite() {
                ticks.push(frequency);
            }
        }
    }
    if ticks.len() < 2 {
        ticks = linear_ticks(min, max)
            .into_iter()
            .filter(|x| usable_x(true, *x))
            .collect();
    }
    ticks
}

/// Full data extent over all drawable series, with 5% linear Y headroom.
fn data_range(opts: &PlotOptions) -> Option<(f64, f64, f64, f64)> {
    let mut range: Option<(f64, f64, f64, f64)> = None;
    for &(x, y) in opts.series.iter().flat_map(|s| &s.points) {
        if !usable_x(opts.log_x, x) || !y.is_finite() {
            continue;
        }
        range = Some(match range {
            None => (x, x, y, y),
            Some((x0, x1, y0, y1)) => (x0.min(x), x1.max(x), y0.min(y), y1.max(y)),
        });
    }
    range.map(|(mut x0, mut x1, y0, y1)| {
        if x0 == x1 {
            let axis = x_to_axis(opts.log_x, x0);
            let pad = if opts.log_x {
                0.05
            } else {
                (x0.abs() * 0.05).max(1.0)
            };
            x0 = x_from_axis(opts.log_x, axis - pad);
            x1 = x_from_axis(opts.log_x, axis + pad);
        }
        let pad = ((y1 - y0) * 0.05).max(1.0);
        (x0, x1, y0 - pad, y1 + pad)
    })
}

/// Reset to all drawable data. Does nothing when no valid data exists.
pub fn fit_view(view: &mut PlotView, opts: &PlotOptions) {
    if let Some((x_min, x_max, y_min, y_max)) = data_range(opts) {
        view.reset(x_min, x_max, y_min, y_max);
    }
}

/// A linear view may include DC. Choose valid log bounds on mode change,
/// without shifting or clamping any measured frequency.
fn ensure_x_view(view: &mut PlotView, opts: &PlotOptions) {
    let valid = |v: &PlotView| {
        usable_x(opts.log_x, v.x_min) && usable_x(opts.log_x, v.x_max) && v.x_min < v.x_max
    };
    if valid(view) {
        return;
    }
    fit_view(view, opts);
    if !valid(view) {
        let max = if view.x_max.is_finite() {
            view.x_max.max(10.0)
        } else {
            10.0
        };
        view.x_min = if opts.log_x { max / 10.0 } else { 0.0 };
        view.x_max = max;
    }
}

/// Draw the plot and report whether the user locked or reset the view.
pub fn show(ui: &mut egui::Ui, view: &mut PlotView, opts: &PlotOptions) -> ViewLock {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        Pos2::new(rect.left() + MARGIN_LEFT, rect.top() + MARGIN_TOP),
        Pos2::new(rect.right() - MARGIN_RIGHT, rect.bottom() - MARGIN_BOTTOM),
    );
    ui.painter().rect_filled(rect, 0.0, BG_COLOR);
    if plot_rect.width() < 2.0 || plot_rect.height() < 2.0 {
        return ViewLock::Unchanged;
    }

    ensure_x_view(view, opts);
    let lock = handle_input(ui, view, opts, plot_rect, &response);
    let mapping = Mapping::new(view, opts.log_x, plot_rect);
    let painter = ui.painter();
    draw_grid(painter, view, opts, &mapping);

    if opts
        .series
        .iter()
        .flat_map(|s| &s.points)
        .any(|&(x, y)| usable_x(opts.log_x, x) && y.is_finite())
    {
        let clipped = painter.with_clip_rect(plot_rect);
        for series in &opts.series {
            draw_series(&clipped, &mapping, series);
        }
        draw_legend(painter, opts, plot_rect);
    } else {
        let has_samples = opts.series.iter().any(|s| !s.points.is_empty());
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            if opts.log_x && has_samples {
                "No data at positive frequencies"
            } else {
                "No data"
            },
            FontId::monospace(14.0),
            TEXT_COLOR,
        );
    }
    draw_cursor(painter, opts, &mapping, &response);
    lock
}

/// Plain wheel zooms X, Shift+wheel zooms Y, Ctrl/Command+wheel zooms
/// both. Drag pans and double-click fits, per reference UI analysis
/// section 5. Only X uses logarithmic axis space.
fn handle_input(
    ui: &egui::Ui,
    view: &mut PlotView,
    opts: &PlotOptions,
    plot_rect: Rect,
    response: &egui::Response,
) -> ViewLock {
    let mut lock = ViewLock::Unchanged;
    let (delta, modifiers, zoom_delta) = ui
        .ctx()
        .input(|i| (i.smooth_scroll_delta, i.modifiers, i.zoom_delta()));
    let zoom_both = modifiers.ctrl || modifiers.command;
    let zoom_y_only = modifiers.shift && !zoom_both;
    let scroll = if zoom_both && zoom_delta != 1.0 {
        // egui converts Ctrl/Command-wheel to zoom and clears scroll deltas.
        zoom_delta.ln()
    } else if (zoom_y_only || zoom_both) && delta.x.abs() > delta.y.abs() {
        delta.x
    } else {
        delta.y
    };
    if scroll != 0.0
        && response.hovered()
        && let Some(pos) = response.hover_pos()
        && plot_rect.contains(pos)
    {
        let mapping = Mapping::new(view, opts.log_x, plot_rect);
        let factor = if scroll > 0.0 { 0.9 } else { 1.0 / 0.9 };
        if zoom_y_only || zoom_both {
            zoom_axis(
                &mut view.y_min,
                &mut view.y_max,
                mapping.value_at(pos.y),
                factor,
            );
        }
        if !zoom_y_only {
            let (mut min, mut max) = (mapping.x_min, mapping.x_min + mapping.x_span);
            let anchor = x_to_axis(opts.log_x, mapping.frequency_at(pos.x));
            zoom_axis(&mut min, &mut max, anchor, factor);
            set_x_bounds(view, opts.log_x, min, max);
        }
        lock = lock.merge(ViewLock::Locked);
    }

    if response.dragged() {
        pan_view(view, opts.log_x, plot_rect, response.drag_delta());
        lock = lock.merge(ViewLock::Locked);
    }

    if response.double_clicked() {
        fit_view(view, opts);
        lock = lock.merge(ViewLock::Unlocked);
    }
    lock
}

fn pan_view(view: &mut PlotView, log_x: bool, rect: Rect, delta: egui::Vec2) {
    let mapping = Mapping::new(view, log_x, rect);
    let dx = -(delta.x as f64 / rect.width() as f64) * mapping.x_span;
    set_x_bounds(
        view,
        log_x,
        mapping.x_min + dx,
        mapping.x_min + mapping.x_span + dx,
    );
    let dy = (delta.y as f64 / rect.height() as f64) * mapping.y_span;
    let (min, max) = (view.y_min + dy, view.y_max + dy);
    if min.is_finite() && max.is_finite() && min < max {
        view.y_min = min;
        view.y_max = max;
    }
}

/// Zoom one axis around the cursor, refusing collapsed or infinite bounds.
fn zoom_axis(min: &mut f64, max: &mut f64, anchor: f64, factor: f64) {
    if (*max - *min) * factor < MIN_SPAN {
        return;
    }
    let new_min = anchor - (anchor - *min) * factor;
    let new_max = anchor + (*max - anchor) * factor;
    if new_min.is_finite() && new_max.is_finite() && new_min < new_max {
        *min = new_min;
        *max = new_max;
    }
}

fn draw_grid(painter: &egui::Painter, view: &PlotView, opts: &PlotOptions, mapping: &Mapping) {
    let rect = mapping.rect;
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let (unit, scale) = si_unit(view.x_min.abs().max(view.x_max.abs()));
    let x_dec = decimals(nice_step(view.x_max - view.x_min, TARGET_DIVISIONS) / scale);
    let mut label_right = f32::NEG_INFINITY;
    for x in frequency_ticks(opts.log_x, view.x_min, view.x_max) {
        let sx = mapping.to_screen(x, view.y_min).x;
        painter.line_segment(
            [Pos2::new(sx, rect.top()), Pos2::new(sx, rect.bottom())],
            Stroke::new(1.0, GRID_COLOR),
        );
        // Broad log views label decades only, with 2/5 minor grid lines.
        let exponent = x_to_axis(opts.log_x, x);
        if opts.log_x && mapping.x_span > 2.0 && (exponent - exponent.round()).abs() > 1e-9 {
            continue;
        }
        let text = if opts.log_x {
            format_freq(x)
        } else {
            format!("{:.*} {}", x_dec, x / scale, unit)
        };
        let galley = painter.layout_no_wrap(text, font.clone(), TEXT_COLOR);
        let left = (sx - galley.size().x / 2.0).clamp(
            rect.left(),
            (rect.right() - galley.size().x).max(rect.left()),
        );
        if left >= label_right + 8.0 {
            label_right = left + galley.size().x;
            painter.galley(Pos2::new(left, rect.bottom() + 3.0), galley, TEXT_COLOR);
        }
    }

    let y_dec = decimals(nice_step(view.y_max - view.y_min, TARGET_DIVISIONS));
    for y in linear_ticks(view.y_min, view.y_max) {
        let sy = mapping.to_screen(view.x_min, y).y;
        painter.line_segment(
            [Pos2::new(rect.left(), sy), Pos2::new(rect.right(), sy)],
            Stroke::new(1.0, GRID_COLOR),
        );
        painter.text(
            Pos2::new(rect.left() - 4.0, sy),
            Align2::RIGHT_CENTER,
            format!("{:.*}", y_dec, y),
            font.clone(),
            TEXT_COLOR,
        );
    }
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        opts.y_label,
        font,
        TEXT_COLOR,
    );
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, GRID_COLOR), StrokeKind::Inside);
}

/// Invalid samples break runs. Negative and zero Y values remain valid.
fn draw_series(painter: &egui::Painter, mapping: &Mapping, series: &Series) {
    let stroke = Stroke::new(1.5, series.color);
    let mut run = Vec::new();
    for &(x, y) in &series.points {
        if usable_x(mapping.log_x, x) && y.is_finite() {
            run.push(mapping.to_screen(x, y));
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

/// Series key: with a single series the shared y_label at the top left
/// is enough; with several series this draws one row per series at the
/// top right (color swatch plus name).
fn draw_legend(painter: &egui::Painter, opts: &PlotOptions, plot_rect: Rect) {
    if opts.series.len() < 2 {
        return;
    }
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let mut y = plot_rect.top() + 2.0;
    for series in &opts.series {
        let label = if opts.y_label.is_empty() {
            series.name.to_string()
        } else {
            format!("{} {}", series.name, opts.y_label)
        };
        painter.text(
            Pos2::new(plot_rect.right() - 18.0, y),
            Align2::RIGHT_TOP,
            label,
            font.clone(),
            TEXT_COLOR,
        );
        let cy = y + LABEL_FONT_SIZE / 2.0 + 1.0;
        painter.line_segment(
            [
                Pos2::new(plot_rect.right() - 14.0, cy),
                Pos2::new(plot_rect.right() - 4.0, cy),
            ],
            Stroke::new(2.0, series.color),
        );
        y += LABEL_FONT_SIZE + 4.0;
    }
}

/// Choose the sample nearest to the cursor in screen frequency space.
/// Values remain in original signed Y units. Ties prefer the first sample.
fn nearest_y(points: &[(f64, f64)], x: f64, log_x: bool) -> Option<f64> {
    if !usable_x(log_x, x) {
        return None;
    }
    let axis = x_to_axis(log_x, x);
    points
        .iter()
        .filter(|&&(x, y)| usable_x(log_x, x) && y.is_finite())
        .min_by(|a, b| {
            (x_to_axis(log_x, a.0) - axis)
                .abs()
                .total_cmp(&(x_to_axis(log_x, b.0) - axis).abs())
        })
        .map(|p| p.1)
}

/// Crosshair readouts always report actual frequency and raw Y values.
fn draw_cursor(
    painter: &egui::Painter,
    opts: &PlotOptions,
    mapping: &Mapping,
    response: &egui::Response,
) {
    let Some(pos) = response.hover_pos() else {
        return;
    };
    let rect = mapping.rect;
    if !rect.contains(pos) {
        return;
    }
    painter.line_segment(
        [
            Pos2::new(pos.x, rect.top()),
            Pos2::new(pos.x, rect.bottom()),
        ],
        Stroke::new(0.5, CURSOR_COLOR),
    );
    painter.line_segment(
        [
            Pos2::new(rect.left(), pos.y),
            Pos2::new(rect.right(), pos.y),
        ],
        Stroke::new(0.5, CURSOR_COLOR),
    );
    let freq = mapping.frequency_at(pos.x);
    let font = FontId::monospace(LABEL_FONT_SIZE);

    if opts.series.len() >= 2 {
        let rows: Vec<_> = opts
            .series
            .iter()
            .filter_map(|s| {
                let value = nearest_y(&s.points, freq, opts.log_x)?;
                Some((
                    s.color,
                    format!("{} {} {}", s.name, format_value(value), opts.y_label),
                ))
            })
            .collect();
        let row_h = LABEL_FONT_SIZE + 3.0;
        for (i, (color, text)) in rows.iter().enumerate() {
            let y = rect.bottom() - 4.0 - (rows.len() - 1 - i) as f32 * row_h;
            painter.text(
                Pos2::new(rect.right() - 4.0, y),
                Align2::RIGHT_BOTTOM,
                text,
                font.clone(),
                *color,
            );
        }
        painter.text(
            Pos2::new(rect.left() + 4.0, rect.bottom() - 4.0),
            Align2::LEFT_BOTTOM,
            format_freq(freq),
            font,
            TEXT_COLOR,
        );
    } else {
        painter.text(
            Pos2::new(rect.right() - 4.0, rect.bottom() - 4.0),
            Align2::RIGHT_BOTTOM,
            format!(
                "{}  {} {}",
                format_freq(freq),
                format_value(mapping.value_at(pos.y)),
                opts.y_label
            ),
            font,
            TEXT_COLOR,
        );
    }
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
    fn format_value_trims_and_switches_to_scientific() {
        assert_eq!(format_value(-50.0), "-50");
        assert_eq!(format_value(2.5), "2.5");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(1.5e7), "1.500e7");
        assert_eq!(format_value(2.0e-4), "2.000e-4");
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
    fn view_lock_merge_keeps_the_stronger_report() {
        use ViewLock::*;
        assert_eq!(Unchanged.merge(Unchanged), Unchanged);
        assert_eq!(Unchanged.merge(Locked), Locked);
        assert_eq!(Unchanged.merge(Unlocked), Unlocked);
        assert_eq!(Unlocked.merge(Locked), Locked);
        // A double-click followed by a drag in one frame stays Locked.
        assert_eq!(Unchanged.merge(Unlocked).merge(Locked), Locked);
        // A drag followed by a double-click in one frame stays Locked.
        assert_eq!(Unchanged.merge(Locked).merge(Unlocked), Locked);
        assert_eq!(Unlocked.merge(Unlocked), Unlocked);
    }

    #[test]
    fn nearest_y_hits_exact_points() {
        let points = [(1.0, 10.0), (2.0, 20.0), (3.0, 30.0)];
        assert_eq!(nearest_y(&points, 2.0, false), Some(20.0));
        assert_eq!(nearest_y(&points, 1.0, false), Some(10.0));
    }

    #[test]
    fn nearest_y_picks_the_closer_side_and_left_on_ties() {
        let points = [(1.0, 10.0), (2.0, 20.0), (3.0, 30.0)];
        assert_eq!(nearest_y(&points, 1.6, false), Some(20.0));
        assert_eq!(nearest_y(&points, 2.4, false), Some(20.0));
        assert_eq!(nearest_y(&points, 2.6, false), Some(30.0));
        // Exactly in the middle: the left point wins.
        assert_eq!(nearest_y(&points, 1.5, false), Some(10.0));
    }

    #[test]
    fn nearest_y_snaps_to_boundary_outside_the_range() {
        let points = [(1.0, 10.0), (2.0, 20.0), (3.0, 30.0)];
        assert_eq!(nearest_y(&points, -5.0, false), Some(10.0));
        assert_eq!(nearest_y(&points, 99.0, false), Some(30.0));
        assert_eq!(nearest_y(&[], 1.0, false), None);
    }

    #[test]
    fn nearest_y_skips_non_finite_points() {
        let points = [
            (1.0, 10.0),
            (2.0, f64::NAN),
            (3.0, f64::INFINITY),
            (4.0, 40.0),
        ];
        assert_eq!(nearest_y(&points, 2.0, false), Some(10.0));
        assert_eq!(nearest_y(&points, 2.9, false), Some(40.0));
        let all_bad = [(1.0, f64::NAN), (f64::NAN, 2.0)];
        assert_eq!(nearest_y(&all_bad, 1.0, false), None);
    }

    fn options(points: &[(f64, f64)], log_x: bool) -> PlotOptions<'static> {
        PlotOptions {
            y_label: "ohm",
            log_x,
            series: vec![Series {
                name: "X",
                color: Color32::WHITE,
                points: points.to_vec(),
            }],
        }
    }

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0))
    }

    fn near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= expected.abs().max(1.0) * 1e-6,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn log_x_spaces_decades_equally_and_keeps_y_linear() {
        let view = PlotView::new(1e6, 1e9, -100.0, 100.0);
        let mapping = Mapping::new(&view, true, rect());
        for (x, screen) in [(1e6, 0.0), (1e7, 200.0), (1e8, 400.0), (1e9, 600.0)] {
            near(mapping.to_screen(x, 0.0).x as f64, screen);
            near(mapping.frequency_at(screen as f32), x);
        }
        for (y, screen) in [(-100.0, 400.0), (0.0, 200.0), (100.0, 0.0)] {
            near(mapping.to_screen(1e6, y).y as f64, screen);
            near(mapping.value_at(screen as f32), y);
        }
        let view = PlotView::new(1e6, 1e8, -100.0, 100.0);
        near(Mapping::new(&view, true, rect()).frequency_at(300.0), 1e7);
        near(
            Mapping::new(&view, false, rect()).frequency_at(300.0),
            50.5e6,
        );
    }

    #[test]
    fn frequency_ticks_use_decades_and_actual_si_units() {
        assert_eq!(
            frequency_ticks(true, 1e6, 1e9),
            [1e6, 2e6, 5e6, 1e7, 2e7, 5e7, 1e8, 2e8, 5e8, 1e9],
        );
        assert_eq!(format_freq(1e6), "1 MHz");
        assert_eq!(format_freq(1e9), "1 GHz");
        assert_eq!(format_freq(433.125e6), "433.125 MHz");
        assert_eq!(frequency_ticks(false, 0.0, 100.0), linear_ticks(0.0, 100.0));
        assert!(frequency_ticks(true, 0.0, 1e9).is_empty());
    }

    #[test]
    fn narrow_log_zoom_still_has_distinct_frequency_ticks() {
        for (min, max) in [
            (433e6, 434e6),
            (433e6, 433e6 + 10.0),
            (6.8e9, 6.8e9 + 100.0),
        ] {
            let ticks = frequency_ticks(true, min, max);
            assert!(ticks.len() >= 2);
            assert!(ticks.iter().all(|x| (min..=max).contains(x)));
            assert!(ticks.windows(2).all(|p| p[0] < p[1]));
            let labels: Vec<_> = ticks.iter().map(|&x| format_freq(x)).collect();
            assert!(labels.windows(2).all(|p| p[0] != p[1]));
        }
    }

    #[test]
    fn linear_ticks_normalize_zero_and_bound_iteration() {
        let ticks = linear_ticks(-0.3, 0.3);
        assert!(ticks.iter().any(|&x| x == 0.0 && !x.is_sign_negative()));
        assert!(linear_ticks(1e20, 1e20 + 2e4).len() <= TARGET_DIVISIONS * 2 + 1);
    }

    #[test]
    fn log_fit_keeps_signed_y_and_excludes_invalid_frequencies() {
        let opts = options(
            &[
                (0.0, -900.0),
                (-1.0, 900.0),
                (1e6, -100.0),
                (1e7, 0.0),
                (1e8, 100.0),
                (f64::NAN, 800.0),
                (1e9, f64::NAN),
            ],
            true,
        );
        let mut view = PlotView::new(0.0, 1.0, 0.0, 1.0);
        fit_view(&mut view, &opts);
        assert_eq!(view, PlotView::new(1e6, 1e8, -110.0, 110.0));
    }

    #[test]
    fn linear_fit_keeps_dc_and_covers_all_series() {
        let mut opts = options(&[(1e6, -80.0), (2e6, -20.0)], false);
        opts.series.push(Series {
            name: "T1",
            color: Color32::WHITE,
            points: vec![(0.0, -60.0), (f64::NAN, -200.0), (3e6, f64::NAN)],
        });
        let mut view = PlotView::new(0.0, 1.0, 0.0, 1.0);
        fit_view(&mut view, &opts);
        assert_eq!(view, PlotView::new(0.0, 2e6, -83.0, -17.0));
    }

    #[test]
    fn fit_handles_empty_zero_y_and_single_frequency_traces() {
        let initial = PlotView::new(1.0, 2.0, 3.0, 4.0);
        let mut view = initial;
        fit_view(&mut view, &options(&[], false));
        assert_eq!(view, initial);
        fit_view(&mut view, &options(&[(0.0, 3.0), (-1.0, 2.0)], true));
        assert_eq!(view, initial);
        for log_x in [false, true] {
            fit_view(&mut view, &options(&[(1e6, 0.0)], log_x));
            assert!(view.x_min < 1e6 && view.x_max > 1e6);
            assert_eq!((view.y_min, view.y_max), (-1.0, 1.0));
        }
    }

    #[test]
    fn log_pan_is_multiplicative_in_x_and_additive_in_y() {
        let mut view = PlotView::new(1e6, 1e9, -100.0, 100.0);
        pan_view(&mut view, true, rect(), egui::vec2(-200.0, 100.0));
        near(view.x_min, 1e7);
        near(view.x_max, 1e10);
        assert_eq!((view.y_min, view.y_max), (-50.0, 150.0));
        pan_view(&mut view, true, rect(), egui::vec2(200.0, -100.0));
        near(view.x_min, 1e6);
        near(view.x_max, 1e9);
        assert_eq!((view.y_min, view.y_max), (-100.0, 100.0));
    }

    #[test]
    fn log_bounds_refuse_overflow_underflow_and_repair_dc_view() {
        let initial = PlotView::new(1e6, 1e9, -100.0, 100.0);
        let mut view = initial;
        for (min, max) in [(-400.0, -350.0), (308.0, 310.0), (7.0, 7.0)] {
            set_x_bounds(&mut view, true, min, max);
            assert_eq!(view, initial);
        }
        view.x_min = 0.0;
        ensure_x_view(
            &mut view,
            &options(&[(0.0, -100.0), (1e6, -50.0), (1e9, 0.0)], true),
        );
        assert_eq!((view.x_min, view.x_max), (1e6, 1e9));
        view.x_min = 0.0;
        view.x_max = 0.0;
        ensure_x_view(&mut view, &options(&[], true));
        assert!(view.x_min > 0.0 && view.x_max > view.x_min);
    }

    #[test]
    fn nearest_y_uses_screen_distance_and_retains_negative_and_zero_y() {
        let points = [(0.0, 300.0), (1e6, -100.0), (1e7, 0.0), (f64::NAN, 2.0)];
        assert_eq!(nearest_y(&points, 5e6, true), Some(0.0));
        assert_eq!(nearest_y(&points, 5e6, false), Some(-100.0));
        assert_eq!(nearest_y(&points, 1e6, true), Some(-100.0));
        assert_eq!(nearest_y(&points, 1e7, true), Some(0.0));
        assert_eq!(nearest_y(&points, 0.0, true), None);
        assert_eq!(nearest_y(&points, f64::NAN, false), None);
    }

    fn frame(
        ctx: &egui::Context,
        view: &mut PlotView,
        opts: &PlotOptions,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (ViewLock, Vec<Vec<Pos2>>) {
        let mut lock = ViewLock::Unchanged;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(rect()),
                events,
                time: Some(time),
                ..Default::default()
            },
            |ui| {
                lock = show(ui, view, opts);
            },
        );
        let paths = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                Shape::Path(path) => Some(path.points.clone()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        (lock, paths)
    }

    #[test]
    fn rendered_log_trace_is_continuous_across_zero_y() {
        let opts = options(&[(1e6, -100.0), (1e7, 0.0), (1e8, 100.0)], true);
        let mut view = PlotView::new(0.0, 1.0, 0.0, 1.0);
        fit_view(&mut view, &opts);
        let (_, paths) = frame(&egui::Context::default(), &mut view, &opts, vec![], 0.0);
        assert_eq!(paths.len(), 1);
        let path = &paths[0];
        assert_eq!(path.len(), 3);
        assert!(path.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        near(path[1].x as f64, ((path[0].x + path[2].x) / 2.0) as f64);
        near(path[1].y as f64, ((path[0].y + path[2].y) / 2.0) as f64);
        assert!(path.windows(2).all(|p| p[0].y > p[1].y));
    }

    #[test]
    fn invalid_samples_break_paths_without_clamping_frequencies() {
        for invalid in [(0.0, 0.0), (-1.0, 0.0), (f64::NAN, 0.0), (3e6, f64::NAN)] {
            let opts = options(
                &[(1e6, -10.0), (2e6, 0.0), invalid, (4e6, 0.0), (5e6, 10.0)],
                true,
            );
            let mut view = PlotView::new(1e6, 5e6, -10.0, 10.0);
            let (_, paths) = frame(&egui::Context::default(), &mut view, &opts, vec![], 0.0);
            assert_eq!(paths.len(), 2);
            assert!(paths.iter().all(|p| p.len() == 2));
        }
    }

    #[test]
    fn wheel_modifiers_zoom_the_expected_axes_at_the_cursor() {
        for log_x in [false, true] {
            for modifiers in [
                egui::Modifiers::NONE,
                egui::Modifiers::SHIFT,
                egui::Modifiers::CTRL,
                egui::Modifiers::COMMAND,
            ] {
                let opts = options(&[(1e6, -100.0), (1e9, 100.0)], log_x);
                let original = PlotView::new(1e6, 1e9, -100.0, 100.0);
                let mut view = original;
                let ctx = egui::Context::default();
                let cursor = Pos2::new(300.0, 200.0);
                frame(&ctx, &mut view, &opts, vec![], 0.0);
                frame(
                    &ctx,
                    &mut view,
                    &opts,
                    vec![egui::Event::PointerMoved(cursor)],
                    0.02,
                );
                let (lock, _) = frame(
                    &ctx,
                    &mut view,
                    &opts,
                    vec![
                        egui::Event::ModifiersChanged(modifiers),
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Line,
                            delta: egui::vec2(0.0, 3.0),
                            phase: egui::TouchPhase::Move,
                            modifiers,
                        },
                    ],
                    0.04,
                );
                assert_eq!(
                    lock,
                    ViewLock::Locked,
                    "log_x={log_x}, modifiers={modifiers:?}"
                );
                assert_eq!(view.x_min != original.x_min, !modifiers.shift);
                assert_eq!(
                    view.y_min != original.y_min,
                    modifiers.shift || modifiers.ctrl || modifiers.command
                );
                let plot_rect = Rect::from_min_max(
                    Pos2::new(MARGIN_LEFT, MARGIN_TOP),
                    Pos2::new(600.0 - MARGIN_RIGHT, 400.0 - MARGIN_BOTTOM),
                );
                let before = Mapping::new(&original, log_x, plot_rect);
                let after = Mapping::new(&view, log_x, plot_rect);
                near(after.frequency_at(cursor.x), before.frequency_at(cursor.x));
                near(after.value_at(cursor.y), before.value_at(cursor.y));
            }
        }
    }

    #[test]
    fn double_click_unlocks_and_fits_full_signed_trace() {
        let opts = options(&[(1e6, -100.0), (1e9, 100.0)], true);
        let mut view = PlotView::new(1e7, 1e8, -10.0, 10.0);
        let ctx = egui::Context::default();
        let cursor = Pos2::new(300.0, 200.0);
        frame(&ctx, &mut view, &opts, vec![], 0.0);
        frame(
            &ctx,
            &mut view,
            &opts,
            vec![egui::Event::PointerMoved(cursor)],
            0.02,
        );
        let mut lock = ViewLock::Unchanged;
        for (i, pressed) in [true, false, true, false].into_iter().enumerate() {
            (lock, _) = frame(
                &ctx,
                &mut view,
                &opts,
                vec![egui::Event::PointerButton {
                    pos: cursor,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
                0.04 + i as f64 * 0.02,
            );
        }
        assert_eq!(lock, ViewLock::Unlocked);
        assert_eq!(view, PlotView::new(1e6, 1e9, -110.0, 110.0));
    }
}
