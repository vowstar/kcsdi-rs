// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cartesian trace plot, custom-drawn with `egui::Painter`.
//!
//! Supports multiple series, a linear or base-10 logarithmic frequency
//! axis, and cursor-anchored zoom (reference UI analysis section 6).
//! Y always uses the original signed measurement units.

use crate::i18n::{Text, language};

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind};

/// One drawn trace.
#[derive(Debug, Clone)]
pub struct Series<'a> {
    /// Trace name, shown in the legend when several series share a plot.
    pub name: &'a str,
    pub color: Color32,
    /// Session visibility, toggled by clicking the legend entry.
    pub visible: bool,
    /// (frequency in Hz, value) points. Non-finite values break the polyline.
    /// Log X additionally excludes non-positive frequencies, never signed Y.
    pub points: Vec<(f64, f64)>,
}

/// A user marker remains attached to measured frequency when the view changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub id: u32,
    pub frequency_hz: f64,
    pub selected: bool,
    pub reference: bool,
}

/// What to draw, prepared by the caller each frame.
#[derive(Clone)]
pub struct PlotOptions<'a> {
    /// Y axis unit label (e.g. "dBm", "ohm", "deg").
    pub y_label: &'a str,
    pub log_x: bool,
    pub series: Vec<Series<'a>>,
}

/// One trace's curves and interaction state in a shared Cartesian region.
pub struct CartesianLayer<'a> {
    pub id: u64,
    pub label: String,
    pub view: &'a mut PlotView,
    pub options: PlotOptions<'static>,
    pub markers: &'a mut [Marker],
    pub line_width: f32,
}

impl<'a> PlotOptions<'a> {
    fn visible_series(&self) -> impl Iterator<Item = &Series<'a>> {
        self.series.iter().filter(|series| series.visible)
    }
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
    pub y_divisions: usize,
}

impl PlotView {
    pub fn new(x_min: f64, x_max: f64, y_min: f64, y_max: f64) -> Self {
        Self {
            x_min,
            x_max,
            y_min,
            y_max,
            y_divisions: 8,
        }
    }

    /// Reset the view to the given range (e.g. after parameters changed).
    pub fn reset(&mut self, x_min: f64, x_max: f64, y_min: f64, y_max: f64) {
        let divisions = self.y_divisions;
        *self = Self::new(x_min, x_max, y_min, y_max);
        self.y_divisions = divisions;
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

/// Keep the dark chart palette and use the active light theme's chrome.
pub(super) fn chart_color(ctx: &egui::Context, dark: Color32) -> Color32 {
    let style = ctx.global_style();
    if style.visuals.dark_mode {
        dark
    } else if dark == BG_COLOR {
        style.visuals.extreme_bg_color
    } else if dark == TEXT_COLOR {
        style.visuals.text_color()
    } else {
        style.visuals.widgets.noninteractive.bg_stroke.color
    }
}

/// About ten percent zoom for a conventional 60-point wheel notch.
/// Exponential scaling gives the same result across egui's smoothing frames.
pub(super) fn wheel_factor(delta: f32) -> f64 {
    (-f64::from(delta) * 0.1_f64.ln_1p() / 60.0).exp()
}

/// Shared frequency-axis control for SPEC and S11 cartesian displays.
pub fn log_x_control(ui: &mut egui::Ui, log_x: &mut bool) -> bool {
    ui.checkbox(log_x, language(ui.ctx()).text(Text::LogX))
        .on_hover_text(language(ui.ctx()).text(Text::LogXHelp))
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
            x_span: positive_span(x_to_axis(log_x, view.x_max) - x_min),
            y_min: view.y_min,
            y_span: positive_span(view.y_max - view.y_min),
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

fn positive_span(span: f64) -> f64 {
    if span.is_finite() && span > 0.0 {
        span
    } else {
        MIN_SPAN
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
pub(crate) fn format_value(v: f64) -> String {
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

/// Retain at least the resolution of one division, including narrow views.
fn y_tick_text(value: f64, step: f64) -> String {
    let precision = (2.0 - step.abs().log10().floor()).clamp(0.0, 16.0) as usize;
    let text = format!("{value:.precision$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    if text == "-0" {
        "0".into()
    } else {
        text.into()
    }
}

/// An offset or multiplier keeps narrow-range labels distinct at the normal
/// font size. These annotations affect labels only, never viewport bounds.
fn y_tick_labels(
    painter: &egui::Painter,
    view: &PlotView,
    unit: &str,
) -> (Vec<std::sync::Arc<egui::Galley>>, String) {
    let divisions = view.y_divisions.clamp(2, 30);
    let span = view.y_max - view.y_min;
    let step = span / divisions as f64;
    let labels = |offset: f64, scale: f64| {
        (0..=divisions)
            .map(|i| {
                painter.layout_no_wrap(
                    y_tick_text(
                        (view.y_min - offset + i as f64 * step) / scale,
                        step / scale,
                    ),
                    FontId::monospace(LABEL_FONT_SIZE),
                    chart_color(painter.ctx(), TEXT_COLOR),
                )
            })
            .collect::<Vec<_>>()
    };
    let fits = |labels: &[std::sync::Arc<egui::Galley>]| {
        labels
            .iter()
            .all(|label| label.size().x <= MARGIN_LEFT - 8.0)
            && labels
                .windows(2)
                .all(|pair| pair[0].job.text != pair[1].job.text)
    };
    let mut offset = 0.0;
    let mut scale = 1.0;
    let mut ticks = labels(offset, scale);
    if !fits(&ticks) && view.y_min.abs().max(view.y_max.abs()) > span * 10.0 {
        offset = view.y_min;
        ticks = labels(offset, scale);
    }
    if !fits(&ticks) {
        scale = 10f64.powf(step.log10().floor());
        ticks = labels(offset, scale);
    }
    let caption = match (scale != 1.0, offset != 0.0) {
        (false, false) => unit.to_string(),
        (false, true) => format!("{unit} ({offset:+})"),
        (true, false) => format!("{unit} (x{scale:e})"),
        (true, true) => format!("{unit} (x{scale:e}, {offset:+})"),
    };
    (ticks, caption)
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
    for &(x, y) in opts.visible_series().flat_map(|s| &s.points) {
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
        let magnitude = y0.abs().max(y1.abs());
        let minimum_pad = if magnitude > 0.0 && magnitude < 1.0 {
            magnitude * 0.05
        } else if magnitude == 0.0 && opts.y_label == "s" {
            1e-9
        } else {
            1.0
        };
        let pad = ((y1 - y0) * 0.05).max(minimum_pad);
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
    if let Some((x_min, x_max, _, _)) = data_range(opts) {
        view.x_min = x_min;
        view.x_max = x_max;
    }
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
#[cfg(test)]
pub fn show(ui: &mut egui::Ui, view: &mut PlotView, opts: &mut PlotOptions) -> ViewLock {
    show_with_markers(ui, view, opts, &mut [])
}

/// Draw every layer with its own Y mapping and one shared frequency axis.
/// View gestures use the selected visible layer. Only its Y bounds change.
pub fn show_multi(
    ui: &mut egui::Ui,
    layers: &mut [CartesianLayer<'_>],
    selected: Option<u64>,
) -> Vec<(u64, ViewLock)> {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        rect.left_top() + egui::vec2(MARGIN_LEFT, MARGIN_TOP),
        rect.right_bottom() - egui::vec2(MARGIN_RIGHT, MARGIN_BOTTOM),
    );
    ui.painter()
        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
    ui.painter()
        .rect_filled(plot_rect, 0.0, chart_color(ui.ctx(), BG_COLOR));
    let mut locks: Vec<_> = layers
        .iter()
        .map(|layer| (layer.id, ViewLock::Unchanged))
        .collect();
    if !plot_rect.is_positive() {
        return locks;
    }
    let Some(active) = active_layer(layers, selected) else {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            language(ui.ctx()).text(Text::NoData),
            FontId::monospace(14.0),
            chart_color(ui.ctx(), TEXT_COLOR),
        );
        return locks;
    };
    let layer = &mut layers[active];
    ensure_x_view(layer.view, &layer.options);
    sync_layer_x(layers, active);
    let legend = multi_legend_rect(ui.painter(), layers, plot_rect);
    let over_legend = response.hover_pos().is_some_and(|pos| legend.contains(pos));
    let mut marker_input = false;
    if !over_legend {
        // Last hit target wins at overlapping marker positions.
        for index in (0..layers.len())
            .filter(|&index| index != active)
            .chain(std::iter::once(active))
        {
            let layer = &mut layers[index];
            marker_input |= ui
                .push_id(("cartesian_layer", layer.id), |ui| {
                    interact_markers(
                        ui,
                        &Mapping::new(layer.view, layer.options.log_x, plot_rect),
                        &layer.options,
                        layer.markers,
                    )
                })
                .inner;
        }
    }
    if !over_legend && !marker_input {
        let layer = &mut layers[active];
        locks[active].1 = handle_input(ui, layer.view, &layer.options, plot_rect, &response);
    }
    sync_layer_x(layers, active);
    let layer = &layers[active];
    let mapping = Mapping::new(layer.view, layer.options.log_x, plot_rect);
    let caption = format!("{} {}", layer.label, layer.options.y_label);
    draw_grid(
        ui.painter(),
        layer.view,
        &PlotOptions {
            y_label: caption.trim(),
            log_x: layer.options.log_x,
            series: Vec::new(),
        },
        &mapping,
    );
    let clipped = ui.painter().with_clip_rect(plot_rect);
    let mut has_data = false;
    for index in (0..layers.len())
        .filter(|&index| index != active)
        .chain(std::iter::once(active))
    {
        let layer = &layers[index];
        let mapping = Mapping::new(layer.view, layer.options.log_x, plot_rect);
        for series in layer.options.visible_series() {
            has_data |= series
                .points
                .iter()
                .any(|&(x, y)| usable_x(layer.options.log_x, x) && y.is_finite());
            draw_series_width(&clipped, &mapping, series, layer.line_width);
        }
        if layer.options.visible_series().any(|s| !s.points.is_empty()) {
            draw_markers_label(&clipped, &mapping, layer.markers, &layer.label);
        }
    }
    if !has_data {
        let has_series = layers.iter().any(|layer| !layer.options.series.is_empty());
        let any_visible = layers
            .iter()
            .any(|layer| layer.options.visible_series().next().is_some());
        let has_samples = layers.iter().any(|layer| {
            layer
                .options
                .visible_series()
                .any(|series| !series.points.is_empty())
        });
        clipped.text(
            plot_rect.center(),
            Align2::CENTER_CENTER,
            language(ui.ctx()).text(if has_series && !any_visible {
                Text::AllTracesHidden
            } else if layers[active].options.log_x && has_samples {
                Text::NoPositiveData
            } else {
                Text::NoData
            }),
            FontId::monospace(14.0),
            chart_color(ui.ctx(), TEXT_COLOR),
        );
    }
    if !over_legend {
        draw_cursor(ui.painter(), &layers[active].options, &mapping, &response);
    }
    draw_multi_legend(ui, layers, legend);
    locks
}

fn active_layer(layers: &[CartesianLayer<'_>], selected: Option<u64>) -> Option<usize> {
    let visible = |layer: &CartesianLayer<'_>| layer.options.visible_series().next().is_some();
    layers
        .iter()
        .position(|layer| Some(layer.id) == selected && visible(layer))
        .or_else(|| layers.iter().position(visible))
        .or_else(|| (!layers.is_empty()).then_some(0))
}

fn sync_layer_x(layers: &mut [CartesianLayer<'_>], active: usize) {
    let source = &layers[active];
    let (min, max, log_x) = (source.view.x_min, source.view.x_max, source.options.log_x);
    for layer in layers {
        layer.view.x_min = min;
        layer.view.x_max = max;
        layer.options.log_x = log_x;
    }
}

#[cfg(test)]
pub fn show_with_markers(
    ui: &mut egui::Ui,
    view: &mut PlotView,
    opts: &mut PlotOptions,
    markers: &mut [Marker],
) -> ViewLock {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        Pos2::new(rect.left() + MARGIN_LEFT, rect.top() + MARGIN_TOP),
        Pos2::new(rect.right() - MARGIN_RIGHT, rect.bottom() - MARGIN_BOTTOM),
    );
    ui.painter()
        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
    ui.painter()
        .rect_filled(plot_rect, 0.0, chart_color(ui.ctx(), BG_COLOR));
    if plot_rect.width() < 2.0 || plot_rect.height() < 2.0 {
        return ViewLock::Unchanged;
    }

    ensure_x_view(view, opts);
    let legend = legend_rect(ui.painter(), opts, plot_rect);
    let over_legend = response.hover_pos().is_some_and(|pos| legend.contains(pos));
    let marker_input = interact_markers(
        ui,
        &Mapping::new(view, opts.log_x, plot_rect),
        opts,
        markers,
    );
    let lock = if over_legend || marker_input {
        ViewLock::Unchanged
    } else {
        handle_input(ui, view, opts, plot_rect, &response)
    };
    let mapping = Mapping::new(view, opts.log_x, plot_rect);
    let painter = ui.painter();
    draw_grid(painter, view, opts, &mapping);

    if opts
        .visible_series()
        .flat_map(|s| &s.points)
        .any(|&(x, y)| usable_x(opts.log_x, x) && y.is_finite())
    {
        let clipped = painter.with_clip_rect(plot_rect);
        for series in opts.visible_series() {
            draw_series(&clipped, &mapping, series);
        }
    } else {
        let has_samples = opts.visible_series().any(|s| !s.points.is_empty());
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            if !opts.series.is_empty() && opts.visible_series().next().is_none() {
                language(ui.ctx()).text(Text::AllTracesHidden)
            } else if opts.log_x && has_samples {
                language(ui.ctx()).text(Text::NoPositiveData)
            } else {
                language(ui.ctx()).text(Text::NoData)
            },
            FontId::monospace(14.0),
            chart_color(painter.ctx(), TEXT_COLOR),
        );
    }
    if !over_legend {
        draw_cursor(painter, opts, &mapping, &response);
    }
    draw_legend(ui, opts, legend);
    if opts
        .visible_series()
        .any(|series| !series.points.is_empty())
    {
        draw_markers(ui.painter(), &mapping, markers);
    }
    lock
}

fn interact_markers(
    ui: &egui::Ui,
    mapping: &Mapping,
    opts: &PlotOptions,
    markers: &mut [Marker],
) -> bool {
    if !opts
        .visible_series()
        .any(|series| !series.points.is_empty())
    {
        return false;
    }
    let mut selected = None;
    let mut busy = false;
    for marker in markers.iter_mut() {
        if !usable_x(opts.log_x, marker.frequency_hz) {
            continue;
        }
        let x = mapping.to_screen(marker.frequency_hz, mapping.y_min).x;
        if x < mapping.rect.left() || x > mapping.rect.right() {
            continue;
        }
        let rect = Rect::from_center_size(
            Pos2::new(x, mapping.rect.top() + 12.0),
            egui::vec2(30.0, 24.0),
        );
        let response = ui
            .interact(
                rect,
                ui.id().with(("marker", marker.id)),
                Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        busy |= response.hovered() || response.dragged();
        if response.clicked() || response.dragged() {
            selected = Some(marker.id);
        }
        if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let frequency =
                mapping.frequency_at(pos.x.clamp(mapping.rect.left(), mapping.rect.right()));
            if let Some((x, _)) = opts
                .visible_series()
                .flat_map(|s| &s.points)
                .filter(|(x, y)| usable_x(opts.log_x, *x) && y.is_finite())
                .min_by(|a, b| (a.0 - frequency).abs().total_cmp(&(b.0 - frequency).abs()))
            {
                marker.frequency_hz = *x;
            }
        }
    }
    if let Some(id) = selected {
        for marker in markers {
            marker.selected = marker.id == id;
        }
    }
    busy
}

#[cfg(test)]
fn draw_markers(painter: &egui::Painter, mapping: &Mapping, markers: &[Marker]) {
    draw_markers_label(painter, mapping, markers, "");
}

fn draw_markers_label(painter: &egui::Painter, mapping: &Mapping, markers: &[Marker], label: &str) {
    let painter = painter.with_clip_rect(mapping.rect);
    for marker in markers {
        if !usable_x(mapping.log_x, marker.frequency_hz) {
            continue;
        }
        let x = mapping.to_screen(marker.frequency_hz, mapping.y_min).x;
        let color = if marker.selected {
            painter.ctx().global_style().visuals.selection.stroke.color
        } else {
            chart_color(painter.ctx(), TEXT_COLOR)
        };
        painter.line_segment(
            [
                Pos2::new(x, mapping.rect.top() + 24.0),
                Pos2::new(x, mapping.rect.bottom()),
            ],
            Stroke::new(0.7, color),
        );
        painter.text(
            Pos2::new(x, mapping.rect.top() + 3.0),
            Align2::CENTER_TOP,
            format!(
                "{label}{}M{}{}",
                if label.is_empty() { "" } else { " " },
                marker.id,
                if marker.reference { "R" } else { "" }
            ),
            FontId::monospace(12.0),
            color,
        );
    }
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
        let factor = if zoom_both && zoom_delta != 1.0 {
            f64::from(zoom_delta).recip()
        } else {
            wheel_factor(scroll)
        };
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
            Stroke::new(1.0, chart_color(painter.ctx(), GRID_COLOR)),
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
        let galley =
            painter.layout_no_wrap(text, font.clone(), chart_color(painter.ctx(), TEXT_COLOR));
        let left = (sx - galley.size().x / 2.0).clamp(
            rect.left(),
            (rect.right() - galley.size().x).max(rect.left()),
        );
        if left >= label_right + 8.0 {
            label_right = left + galley.size().x;
            painter.galley(
                Pos2::new(left, rect.bottom() + 3.0),
                galley,
                chart_color(painter.ctx(), TEXT_COLOR),
            );
        }
    }

    let divisions = view.y_divisions.clamp(2, 30);
    let step = (view.y_max - view.y_min) / divisions as f64;
    let (labels, caption) = y_tick_labels(painter, view, opts.y_label);
    for (index, label) in labels.into_iter().enumerate() {
        let y = view.y_min + index as f64 * step;
        let sy = mapping.to_screen(view.x_min, y).y;
        painter.line_segment(
            [Pos2::new(rect.left(), sy), Pos2::new(rect.right(), sy)],
            Stroke::new(1.0, chart_color(painter.ctx(), GRID_COLOR)),
        );
        let left = (rect.left() - 4.0 - label.size().x).max(rect.left() - MARGIN_LEFT + 2.0);
        painter.galley(
            Pos2::new(left, sy - label.size().y / 2.0),
            label,
            chart_color(painter.ctx(), TEXT_COLOR),
        );
    }
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        caption,
        font,
        chart_color(painter.ctx(), TEXT_COLOR),
    );
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, chart_color(painter.ctx(), GRID_COLOR)),
        StrokeKind::Inside,
    );
}

/// Invalid samples break runs. Negative and zero Y values remain valid.
#[cfg(test)]
fn draw_series(painter: &egui::Painter, mapping: &Mapping, series: &Series) {
    draw_series_width(painter, mapping, series, 1.5);
}

pub(super) fn stroke_width(width: f32) -> f32 {
    if width.is_finite() && width > 0.0 {
        width
    } else {
        1.5
    }
}

fn draw_series_width(painter: &egui::Painter, mapping: &Mapping, series: &Series, width: f32) {
    let stroke = Stroke::new(stroke_width(width), series.color);
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

const LEGEND_ROW_HEIGHT: f32 = LABEL_FONT_SIZE + 8.0;

fn series_label(name: &str, unit: &str) -> String {
    if unit.is_empty() {
        name.to_string()
    } else {
        format!("{name} {unit}")
    }
}

fn layer_series_label(layer: &CartesianLayer<'_>, series: &Series<'_>) -> String {
    format!(
        "{} {}",
        layer.label,
        series_label(series.name, layer.options.y_label)
    )
}

fn multi_legend_rect(painter: &egui::Painter, layers: &[CartesianLayer<'_>], plot: Rect) -> Rect {
    let mut count = 0;
    let mut width = 0.0_f32;
    for layer in layers {
        for series in &layer.options.series {
            count += 1;
            width = width.max(
                painter
                    .layout_no_wrap(
                        layer_series_label(layer, series),
                        FontId::monospace(LABEL_FONT_SIZE),
                        chart_color(painter.ctx(), TEXT_COLOR),
                    )
                    .size()
                    .x,
            );
        }
    }
    if count == 0 {
        return Rect::NOTHING;
    }
    Rect::from_min_max(
        Pos2::new(
            (plot.right() - width - 28.0).max(plot.left()),
            plot.top() + 2.0,
        ),
        Pos2::new(
            plot.right(),
            (plot.top() + 10.0 + LEGEND_ROW_HEIGHT * count as f32).min(plot.bottom()),
        ),
    )
}

/// Scroll the legend independently so every component remains reachable.
fn draw_multi_legend(ui: &egui::Ui, layers: &mut [CartesianLayer<'_>], rect: Rect) {
    if !rect.is_positive() {
        return;
    }
    let count: usize = layers.iter().map(|layer| layer.options.series.len()).sum();
    let overflow = (8.0 + LEGEND_ROW_HEIGHT * count as f32 - rect.height()).max(0.0);
    let scroll_id = ui.id().with("cartesian_legend_scroll");
    let mut offset = ui
        .ctx()
        .data(|data| data.get_temp::<f32>(scroll_id))
        .unwrap_or(0.0);
    if ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|pos| rect.contains(pos))
    {
        offset -= ui.ctx().input(|input| input.smooth_scroll_delta.y);
    }
    offset = offset.clamp(0.0, overflow);
    ui.ctx()
        .data_mut(|data| data.insert_temp(scroll_id, offset));
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 2.0, chart_color(ui.ctx(), BG_COLOR));
    let mut index = 0;
    for layer in layers {
        for (component, series) in layer.options.series.iter_mut().enumerate() {
            let top = rect.top() + 4.0 + index as f32 * LEGEND_ROW_HEIGHT - offset;
            index += 1;
            let row = Rect::from_min_max(
                Pos2::new(rect.left(), top),
                Pos2::new(rect.right(), top + LEGEND_ROW_HEIGHT),
            );
            if !row.intersects(rect) {
                continue;
            }
            legend_row(
                ui,
                &painter,
                row,
                ui.id().with(("cartesian_legend", layer.id, component)),
                &format!(
                    "{} {}",
                    layer.label,
                    series_label(series.name, layer.options.y_label)
                ),
                series.color,
                &mut series.visible,
            );
        }
    }
    if overflow > 0.0 {
        let available = rect.height() - 4.0;
        let height = (available * rect.height() / (rect.height() + overflow)).max(12.0);
        let top = rect.top() + 2.0 + (available - height) * offset / overflow;
        painter.line_segment(
            [
                Pos2::new(rect.left() + 2.0, top),
                Pos2::new(rect.left() + 2.0, top + height),
            ],
            Stroke::new(2.0, chart_color(ui.ctx(), TEXT_COLOR)),
        );
    }
}

fn legend_row(
    ui: &egui::Ui,
    painter: &egui::Painter,
    row: Rect,
    id: egui::Id,
    label: &str,
    color: Color32,
    visible: &mut bool,
) {
    let response = ui
        .interact(
            row.intersect(painter.clip_rect()),
            id,
            Sense::click_and_drag(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(language(ui.ctx()).text(if *visible {
            Text::HideTrace
        } else {
            Text::ShowTrace
        }));
    if response.clicked()
        && response
            .interact_pointer_pos()
            .is_some_and(|pos| painter.clip_rect().contains(pos))
    {
        *visible = !*visible;
    }
    if response.hovered() {
        painter.rect_filled(row, 2.0, chart_color(ui.ctx(), GRID_COLOR));
    }
    let color = if *visible {
        color
    } else {
        Color32::from_gray(100)
    };
    let bounds = painter.text(
        Pos2::new(row.right() - 18.0, row.center().y),
        Align2::RIGHT_CENTER,
        label,
        FontId::monospace(LABEL_FONT_SIZE),
        if *visible {
            chart_color(ui.ctx(), TEXT_COLOR)
        } else {
            color
        },
    );
    painter.line_segment(
        [
            Pos2::new(row.right() - 14.0, row.center().y),
            Pos2::new(row.right() - 4.0, row.center().y),
        ],
        Stroke::new(2.0, color),
    );
    if !*visible {
        painter.line_segment(
            [bounds.left_center(), bounds.right_center()],
            Stroke::new(1.0, color),
        );
    }
}

/// Reserve the same rectangle for painting and hit-testing.
#[cfg(test)]
fn legend_rect(painter: &egui::Painter, opts: &PlotOptions, plot_rect: Rect) -> Rect {
    if opts.series.len() < 2 {
        return Rect::NOTHING;
    }
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let width = opts
        .series
        .iter()
        .map(|series| {
            painter
                .layout_no_wrap(
                    series_label(series.name, opts.y_label),
                    font.clone(),
                    chart_color(painter.ctx(), TEXT_COLOR),
                )
                .size()
                .x
        })
        .fold(0.0_f32, f32::max)
        + 28.0;
    Rect::from_min_max(
        Pos2::new(
            (plot_rect.right() - width).max(plot_rect.left()),
            plot_rect.top() + 2.0,
        ),
        Pos2::new(
            plot_rect.right(),
            plot_rect.top() + 10.0 + LEGEND_ROW_HEIGHT * opts.series.len() as f32,
        ),
    )
    .intersect(plot_rect)
}

/// Keep every legend entry reachable, including when all traces are hidden.
#[cfg(test)]
fn draw_legend(ui: &egui::Ui, opts: &mut PlotOptions, rect: Rect) {
    if opts.series.len() < 2 || !rect.is_positive() {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 2.0, chart_color(painter.ctx(), BG_COLOR));
    for (index, series) in opts.series.iter_mut().enumerate() {
        let top = rect.top() + 4.0 + index as f32 * LEGEND_ROW_HEIGHT;
        let row = Rect::from_min_max(
            Pos2::new(rect.left(), top),
            Pos2::new(rect.right(), top + LEGEND_ROW_HEIGHT),
        )
        .intersect(rect);
        if !row.is_positive() {
            continue;
        }
        legend_row(
            ui,
            &painter,
            row,
            ui.id().with(("trace_legend", index, series.name)),
            &series_label(series.name, opts.y_label),
            series.color,
            &mut series.visible,
        );
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
        Stroke::new(0.5, chart_color(painter.ctx(), CURSOR_COLOR)),
    );
    painter.line_segment(
        [
            Pos2::new(rect.left(), pos.y),
            Pos2::new(rect.right(), pos.y),
        ],
        Stroke::new(0.5, chart_color(painter.ctx(), CURSOR_COLOR)),
    );
    let freq = mapping.frequency_at(pos.x);
    let font = FontId::monospace(LABEL_FONT_SIZE);

    if opts.series.len() >= 2 {
        let rows: Vec<_> = opts
            .visible_series()
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
            chart_color(painter.ctx(), TEXT_COLOR),
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
            chart_color(painter.ctx(), TEXT_COLOR),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multi_frame(
        ctx: &egui::Context,
        layers: &mut [CartesianLayer<'_>],
        selected: Option<u64>,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (Vec<(u64, ViewLock)>, egui::FullOutput) {
        let mut locks = Vec::new();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(rect()),
                events,
                time: Some(time),
                ..Default::default()
            },
            |ui| locks = show_multi(ui, layers, selected),
        );
        (locks, output)
    }

    fn test_layer<'a>(
        id: u64,
        view: &'a mut PlotView,
        points: &[(f64, f64)],
    ) -> CartesianLayer<'a> {
        CartesianLayer {
            id,
            label: format!("T{id}"),
            view,
            options: options(points, false),
            markers: &mut [],
            line_width: id as f32,
        }
    }

    #[test]
    fn multiple_layers_share_x_but_render_with_their_own_y_and_style() {
        let mut first = PlotView::new(7e6, 9e6, 0.0, 100.0);
        let mut second = PlotView::new(1e6, 3e6, -100.0, -20.0);
        let mut layers = [
            test_layer(2, &mut first, &[(1e6, 25.0), (3e6, 75.0)]),
            test_layer(4, &mut second, &[(1e6, -80.0), (3e6, -40.0)]),
        ];
        layers[1].options.y_label = "dBm";
        let (locks, output) =
            multi_frame(&egui::Context::default(), &mut layers, Some(4), vec![], 0.0);
        let paths: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::Path(path) => Some((path.stroke.width, path.points.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(paths.len(), 2);
        assert_eq!((paths[0].0, paths[1].0), (2.0, 4.0));
        assert_eq!(paths[0].1, paths[1].1);
        assert!(output.shapes.iter().any(
            |shape| matches!(&shape.shape, Shape::Text(text) if text.galley.text() == "T4 dBm")
        ));
        assert_eq!(locks, [(2, ViewLock::Unchanged), (4, ViewLock::Unchanged)]);
        assert_eq!((layers[0].view.x_min, layers[0].view.x_max), (1e6, 3e6));
        assert_eq!((layers[0].view.y_min, layers[0].view.y_max), (0.0, 100.0));
        assert_eq!(
            (layers[1].view.y_min, layers[1].view.y_max),
            (-100.0, -20.0)
        );
        output.drop_without_applying_deltas();
    }

    #[test]
    fn multi_zoom_changes_only_selected_y_and_synchronizes_x() {
        for modifiers in [
            egui::Modifiers::NONE,
            egui::Modifiers::SHIFT,
            egui::Modifiers::CTRL,
        ] {
            let ctx = egui::Context::default();
            let mut first = PlotView::new(1e6, 3e6, 0.0, 100.0);
            let mut second = PlotView::new(1e6, 3e6, -100.0, -20.0);
            let mut layers = [
                test_layer(2, &mut first, &[(1e6, 25.0), (3e6, 75.0)]),
                test_layer(4, &mut second, &[(1e6, -80.0), (3e6, -40.0)]),
            ];
            multi_frame(&ctx, &mut layers, Some(4), vec![], 0.0)
                .1
                .drop_without_applying_deltas();
            multi_frame(
                &ctx,
                &mut layers,
                Some(4),
                vec![egui::Event::PointerMoved(Pos2::new(300.0, 200.0))],
                0.02,
            )
            .1
            .drop_without_applying_deltas();
            let (locks, output) = multi_frame(
                &ctx,
                &mut layers,
                Some(4),
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
            assert_eq!(locks, [(2, ViewLock::Unchanged), (4, ViewLock::Locked)]);
            assert_eq!((layers[0].view.y_min, layers[0].view.y_max), (0.0, 100.0));
            assert_eq!(
                layers[1].view.y_min != -100.0,
                modifiers.shift || modifiers.ctrl
            );
            assert_eq!(layers[0].view.x_min, layers[1].view.x_min);
            assert_eq!(layers[0].view.x_max, layers[1].view.x_max);
            assert_eq!(layers[0].view.x_min != 1e6, !modifiers.shift);
            output.drop_without_applying_deltas();
        }
    }

    fn click_multi_text(
        ctx: &egui::Context,
        layers: &mut [CartesianLayer<'_>],
        label: &str,
        time: f64,
    ) {
        let (_, output) = multi_frame(ctx, layers, Some(2), vec![], time);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing layer label {label}"));
        output.drop_without_applying_deltas();
        for (frame, events) in [
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        ]
        .into_iter()
        .enumerate()
        {
            multi_frame(
                ctx,
                layers,
                Some(2),
                events,
                time + (frame + 1) as f64 * 0.02,
            )
            .1
            .drop_without_applying_deltas();
        }
    }

    #[test]
    fn duplicate_component_names_have_independent_legend_hits_and_can_be_restored() {
        let ctx = egui::Context::default();
        let mut first = PlotView::new(1e6, 3e6, 0.0, 100.0);
        let mut second = first;
        let mut layers = [
            test_layer(2, &mut first, &[(1e6, 25.0), (3e6, 75.0)]),
            test_layer(4, &mut second, &[(1e6, 30.0), (3e6, 70.0)]),
        ];
        click_multi_text(&ctx, &mut layers, "T4 X ohm", 0.0);
        assert!(layers[0].options.series[0].visible);
        assert!(!layers[1].options.series[0].visible);
        click_multi_text(&ctx, &mut layers, "T2 X ohm", 1.0);
        assert!(!layers[0].options.series[0].visible);
        click_multi_text(&ctx, &mut layers, "T4 X ohm", 2.0);
        assert!(!layers[0].options.series[0].visible);
        assert!(layers[1].options.series[0].visible);
        assert_eq!((layers[0].view.y_min, layers[0].view.y_max), (0.0, 100.0));
    }

    #[test]
    fn overlapping_marker_numbers_prefer_the_selected_trace() {
        let ctx = egui::Context::default();
        let mut first = PlotView::new(1e6, 3e6, 0.0, 100.0);
        let mut second = first;
        let marker = |frequency_hz| {
            [Marker {
                id: 1,
                frequency_hz,
                selected: false,
                reference: false,
            }]
        };
        let mut first_markers = marker(1.5e6);
        let mut second_markers = marker(1.5e6);
        let mut layers = [
            test_layer(2, &mut first, &[(1e6, 25.0), (3e6, 75.0)]),
            test_layer(4, &mut second, &[(1e6, 30.0), (3e6, 70.0)]),
        ];
        layers[0].markers = &mut first_markers;
        layers[1].markers = &mut second_markers;
        click_multi_text(&ctx, &mut layers, "T2 M1", 0.0);
        assert!(layers[0].markers[0].selected);
        assert!(!layers[1].markers[0].selected);
    }

    #[test]
    fn empty_multi_plot_and_invalid_widths_are_safe() {
        let (locks, output) = multi_frame(&egui::Context::default(), &mut [], None, vec![], 0.0);
        assert!(locks.is_empty());
        assert!(output.shapes.iter().any(
            |shape| matches!(&shape.shape, Shape::Text(text) if text.galley.text() == "No data")
        ));
        output.drop_without_applying_deltas();
        for width in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            assert_eq!(stroke_width(width), 1.5);
        }
    }

    #[test]
    fn unmeasured_layers_show_no_data_instead_of_all_hidden() {
        for (has_series, visible, expected) in [
            (false, true, Text::NoData),
            (true, true, Text::NoData),
            (true, false, Text::AllTracesHidden),
        ] {
            let ctx = egui::Context::default();
            let mut first = PlotView::new(1e6, 3e6, 0.0, 100.0);
            let mut second = first;
            let mut layers = [
                test_layer(2, &mut first, &[]),
                test_layer(4, &mut second, &[]),
            ];
            for layer in &mut layers {
                if has_series {
                    layer.options.series[0].visible = visible;
                } else {
                    layer.options.series.clear();
                }
            }
            let (_, output) = multi_frame(&ctx, &mut layers, Some(4), vec![], 0.0);
            let messages: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .filter(|text| {
                    [Text::NoData, Text::AllTracesHidden, Text::NoPositiveData]
                        .iter()
                        .any(|message| *text == language(&ctx).text(*message))
                })
                .collect();
            assert_eq!(messages, [language(&ctx).text(expected)]);
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn a_clipped_legend_row_cannot_receive_clicks_outside_its_clip() {
        let ctx = egui::Context::default();
        let mut visible = true;
        let mut time = 0.0;
        for (pos, expected) in [
            (Pos2::new(150.0, 95.0), true),
            (Pos2::new(150.0, 105.0), false),
        ] {
            let mut plot_clicked = false;
            for events in [
                vec![],
                vec![egui::Event::PointerMoved(pos)],
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }],
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ] {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(rect()),
                        events,
                        time: Some(time),
                        ..Default::default()
                    },
                    |ui| {
                        let (_, background) =
                            ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());
                        plot_clicked |= background.clicked();
                        let painter = ui.painter().with_clip_rect(Rect::from_min_max(
                            Pos2::new(100.0, 100.0),
                            Pos2::new(200.0, 120.0),
                        ));
                        legend_row(
                            ui,
                            &painter,
                            Rect::from_min_max(Pos2::new(100.0, 90.0), Pos2::new(200.0, 110.0)),
                            ui.id().with("clipped_row"),
                            "T1",
                            Color32::WHITE,
                            &mut visible,
                        );
                    },
                )
                .drop_without_applying_deltas();
                time += 0.02;
            }
            assert_eq!(visible, expected);
            assert_eq!(plot_clicked, expected);
        }
    }

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
    fn non_round_y_limits_keep_tick_labels_inside_the_fixed_axis_margin() {
        let ctx = egui::Context::default();
        let mut view = PlotView::new(1e6, 5e6, -454.484830, 461.371430);
        let original = view;
        let mut opts = options(&[(1e6, -400.7), (5e6, 430.125)], false);
        let (_, output) = widget_frame(&ctx, &mut view, &mut opts, vec![], 0.0);
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::Text(text)
                    if text.pos.x < MARGIN_LEFT && text.galley.job.text.parse::<f64>().is_ok() =>
                {
                    Some((
                        text.galley.job.text.clone(),
                        text.galley.rect.translate(text.pos.to_vec2()),
                    ))
                }
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        assert_eq!(labels.len(), 9);
        assert!(labels.iter().all(|(text, rect)| !text.contains('.')
            && rect.left() >= 0.0
            && rect.right() < MARGIN_LEFT));
        assert_eq!(view, original);
    }

    #[test]
    fn narrow_y_ranges_keep_distinct_legible_labels_without_changing_bounds() {
        for (min, max) in [
            (1000.0, 1000.008),
            (-1000.008, -1000.0),
            (1e9, 1e9 + 0.008),
            (0.0, 8e-18),
            (-9e-6, -1e-6),
        ] {
            let ctx = egui::Context::default();
            let view = PlotView::new(1e6, 5e6, min, max);
            let output = ctx.run_ui(Default::default(), |ui| {
                let (ticks, caption) = y_tick_labels(ui.painter(), &view, "ohm");
                assert_eq!(ticks.len(), 9);
                assert!(
                    ticks
                        .windows(2)
                        .all(|pair| pair[0].job.text != pair[1].job.text)
                );
                assert!(ticks.iter().all(|label| {
                    label.size().x <= MARGIN_LEFT - 8.0
                        && label.job.sections[0].format.font_id.size == LABEL_FONT_SIZE
                }));
                assert!(caption.starts_with("ohm ("), "{caption}");
            });
            output.drop_without_applying_deltas();
            assert_eq!((view.y_min, view.y_max), (min, max));
        }

        let mut view = PlotView::new(1e6, 5e6, 1000.0, 1000.008);
        let original = view;
        let mut opts = options(&[(1e6, 1000.003), (5e6, 1000.005)], false);
        let (_, output) =
            widget_frame(&egui::Context::default(), &mut view, &mut opts, vec![], 0.0);
        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, Shape::Text(text) if text.galley.job.text.ends_with("(+1000)"))
        }));
        output.drop_without_applying_deltas();
        assert_eq!(view, original);
    }

    #[test]
    fn delay_tick_multipliers_keep_fractional_steps_and_signed_values() {
        for (low, high, expected) in [
            (
                -13.8e-9,
                3.8e-9,
                [
                    "-13.8", "-11.6", "-9.4", "-7.2", "-5", "-2.8", "-0.6", "1.6", "3.8",
                ],
            ),
            (
                -50e-9,
                50e-9,
                [
                    "-5", "-3.75", "-2.5", "-1.25", "0", "1.25", "2.5", "3.75", "5",
                ],
            ),
        ] {
            let ctx = egui::Context::default();
            let view = PlotView::new(1e6, 100e6, low, high);
            ctx.run_ui(Default::default(), |ui| {
                let (ticks, caption) = y_tick_labels(ui.painter(), &view, "s");
                assert!(caption.starts_with("s (x1e-"));
                let text: Vec<_> = ticks.iter().map(|tick| tick.text()).collect();
                assert_eq!(text, expected);
                assert!(ticks.iter().all(|tick| tick.size().x <= MARGIN_LEFT - 8.0));
            })
            .drop_without_applying_deltas();
        }
    }

    #[test]
    fn repairing_log_x_from_dc_preserves_manual_y_scale() {
        for points in [vec![(0.0, -10.0), (1e6, -20.0), (5e6, -30.0)], vec![]] {
            let mut opts = options(&points, true);
            let mut view = PlotView::new(0.0, 5e6, -53.25, -12.75);
            view.y_divisions = 13;
            let original = view;
            let (_, output) =
                widget_frame(&egui::Context::default(), &mut view, &mut opts, vec![], 0.0);
            output.drop_without_applying_deltas();
            assert!(view.x_min > 0.0 && view.x_max > view.x_min);
            assert_eq!(view.y_min, original.y_min);
            assert_eq!(view.y_max, original.y_max);
            assert_eq!(view.y_divisions, original.y_divisions);
        }
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
                visible: true,
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
            visible: true,
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
    fn small_scalar_values_fit_without_a_whole_unit_padding() {
        for unit in ["s", "ohm", "dB"] {
            for (low, high) in [(-5e-9, 3e-9), (2e-12, 6e-12), (7e-9, 7e-9)] {
                let mut opts = options(&[(1e6, low), (2e6, high)], false);
                opts.y_label = unit;
                let mut view = PlotView::new(1e6, 2e6, -1.0, 1.0);
                fit_view(&mut view, &opts);
                assert!(view.y_min < low && view.y_max > high);
                assert!((view.y_max - view.y_min) < 2.0 * low.abs().max(high.abs()));
                let mapping = Mapping::new(&view, false, rect());
                assert_eq!(mapping.to_screen(1e6, view.y_min).y, rect().bottom());
                assert_eq!(mapping.to_screen(2e6, view.y_max).y, rect().top());
            }
        }
        let mut opts = options(&[(1e6, 0.0), (2e6, 0.0)], false);
        opts.y_label = "s";
        let mut view = PlotView::new(1e6, 2e6, -1.0, 1.0);
        fit_view(&mut view, &opts);
        assert_eq!((view.y_min, view.y_max), (-1e-9, 1e-9));
    }

    #[test]
    fn sub_nanosecond_views_map_zoom_and_pan_without_clamping() {
        let mut view = PlotView::new(1e6, 2e6, -5e-12, 3e-12);
        let mapping = Mapping::new(&view, false, rect());
        assert_eq!(mapping.to_screen(1e6, -5e-12).y, rect().bottom());
        assert_eq!(mapping.to_screen(2e6, 3e-12).y, rect().top());
        assert!((mapping.value_at(rect().center().y) + 1e-12).abs() < 1e-26);
        zoom_axis(&mut view.y_min, &mut view.y_max, -1e-12, 0.5);
        assert!((view.y_min + 3e-12).abs() < 1e-26);
        assert!((view.y_max - 1e-12).abs() < 1e-26);
        pan_view(
            &mut view,
            false,
            rect(),
            egui::vec2(0.0, rect().height() * 0.25),
        );
        assert!((view.y_min + 2e-12).abs() < 1e-26);
        assert!((view.y_max - 2e-12).abs() < 1e-26);
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

    fn widget_frame(
        ctx: &egui::Context,
        view: &mut PlotView,
        opts: &mut PlotOptions,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (ViewLock, egui::FullOutput) {
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
        (lock, output)
    }

    fn frame(
        ctx: &egui::Context,
        view: &mut PlotView,
        opts: &PlotOptions,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (ViewLock, Vec<Vec<Pos2>>) {
        let (lock, output) = widget_frame(ctx, view, &mut opts.clone(), events, time);
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
    fn one_wheel_event_remains_moderate_after_all_smoothing_frames() {
        let ctx = egui::Context::default();
        let opts = options(&[(1e6, -10.0), (1e9, 10.0)], false);
        let mut view = PlotView::new(1e6, 1e9, -10.0, 10.0);
        let span = view.x_max - view.x_min;
        for index in 0..100 {
            let events = match index {
                1 => vec![egui::Event::PointerMoved(Pos2::new(300.0, 200.0))],
                2 => vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 3.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                }],
                _ => vec![],
            };
            frame(&ctx, &mut view, &opts, events, index as f64 / 60.0);
        }
        let ratio = (view.x_max - view.x_min) / span;
        assert!(
            (0.65..0.99).contains(&ratio),
            "zoom ratio after smoothing: {ratio}"
        );
    }

    #[test]
    fn wheel_scaling_is_independent_of_frame_partition() {
        near(wheel_factor(60.0), wheel_factor(5.0).powi(12));
        near(wheel_factor(60.0) * wheel_factor(-60.0), 1.0);
    }

    #[test]
    fn dragging_a_marker_snaps_frequency_without_panning_the_view() {
        let ctx = egui::Context::default();
        let mut opts = options(&[(1e6, 1.0), (3e6, 2.0), (5e6, 3.0)], false);
        let original = PlotView::new(1e6, 5e6, 0.0, 4.0);
        let mut view = original;
        let mut markers = vec![Marker {
            id: 1,
            frequency_hz: 3e6,
            selected: true,
            reference: false,
        }];
        let from = Pos2::new(320.0, 20.0);
        let to = Pos2::new(540.0, 20.0);
        for (index, events) in [
            vec![],
            vec![egui::Event::PointerMoved(from)],
            vec![egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            vec![egui::Event::PointerMoved(to)],
            vec![egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        ]
        .into_iter()
        .enumerate()
        {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0))),
                    time: Some(index as f64 / 60.0),
                    events,
                    ..Default::default()
                },
                |ui| {
                    show_with_markers(ui, &mut view, &mut opts, &mut markers);
                },
            )
            .drop_without_applying_deltas();
        }
        assert_eq!(markers[0].frequency_hz, 5e6);
        assert_eq!(view, original);
    }

    #[test]
    fn light_chart_uses_the_active_background_and_text_colors() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Light);
        assert_eq!(
            chart_color(&ctx, BG_COLOR),
            ctx.global_style().visuals.extreme_bg_color
        );
        assert_eq!(
            chart_color(&ctx, TEXT_COLOR),
            ctx.global_style().visuals.text_color()
        );
        assert_ne!(chart_color(&ctx, BG_COLOR), BG_COLOR);
    }

    #[test]
    fn hidden_series_do_not_affect_fit_and_all_hidden_keeps_the_view() {
        let mut opts = impedance_options();
        opts.series[0].visible = false;
        opts.series[1].visible = false;
        let mut view = PlotView::new(1.0, 2.0, -1.0, 1.0);
        fit_view(&mut view, &opts);
        assert_eq!(view, PlotView::new(1e6, 1e9, -21.0, -9.0));
        let before = view;
        opts.series[2].visible = false;
        fit_view(&mut view, &opts);
        assert_eq!(view, before);
    }

    fn impedance_options() -> PlotOptions<'static> {
        PlotOptions {
            y_label: "ohm",
            log_x: true,
            series: [
                ("|Z|", 1000.0, 2000.0),
                ("R", 20.0, 40.0),
                ("X", -10.0, -20.0),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (name, a, b))| Series {
                name,
                color: crate::theme::TRACE_COLORS[i],
                visible: true,
                points: vec![(1e6, a), (1e9, b)],
            })
            .collect(),
        }
    }

    fn text_shapes(output: &egui::FullOutput) -> Vec<(String, Pos2)> {
        output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                Shape::Text(text) => Some((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()).center(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_plot_messages_follow_live_language_changes() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut opts = impedance_options();
        for selected in crate::i18n::Language::ALL {
            crate::i18n::set_language(&ctx, selected);
            for series in &mut opts.series {
                series.visible = false;
            }
            let mut view = PlotView::new(1e6, 1e9, -100.0, 100.0);
            let (_, output) = widget_frame(&ctx, &mut view, &mut opts, vec![], 0.0);
            assert!(
                text_shapes(&output)
                    .iter()
                    .any(|(text, _)| text == selected.text(Text::AllTracesHidden))
            );
            output.drop_without_applying_deltas();
        }
    }

    fn click_legend(
        ctx: &egui::Context,
        view: &mut PlotView,
        opts: &mut PlotOptions,
        name: &str,
        time: f64,
    ) -> ViewLock {
        let (_, output) = widget_frame(ctx, view, opts, vec![], time);
        let texts = text_shapes(&output);
        let pos = texts
            .iter()
            .find(|(text, _)| text == &series_label(name, opts.y_label))
            .unwrap()
            .1;
        output.drop_without_applying_deltas();
        let mut lock = ViewLock::Unchanged;
        for (i, events) in [
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        ]
        .into_iter()
        .enumerate()
        {
            let (frame_lock, output) =
                widget_frame(ctx, view, opts, events, time + (i + 1) as f64 * 0.02);
            lock = lock.merge(frame_lock);
            output.drop_without_applying_deltas();
        }
        lock
    }

    #[test]
    fn legend_toggles_each_trace_and_can_recover_after_hiding_all() {
        let ctx = egui::Context::default();
        let mut opts = impedance_options();
        let original_points: Vec<_> = opts.series.iter().map(|s| s.points.clone()).collect();
        let original_view = PlotView::new(1e6, 1e9, -100.0, 100.0);
        let mut view = original_view;
        for (i, name) in ["|Z|", "R", "X"].into_iter().enumerate() {
            assert_eq!(
                click_legend(&ctx, &mut view, &mut opts, name, 1.0 + i as f64),
                ViewLock::Unchanged
            );
            for (j, series) in opts.series.iter().enumerate() {
                assert_eq!(series.visible, j > i);
            }
            let (_, paths) = frame(&ctx, &mut view, &opts, vec![], 1.2 + i as f64);
            assert_eq!(paths.len(), 2 - i);
        }
        let (_, output) = widget_frame(&ctx, &mut view, &mut opts, vec![], 4.0);
        let texts = text_shapes(&output);
        assert!(texts.iter().any(|(text, _)| text == "All traces hidden"));
        for name in ["|Z|", "R", "X"] {
            assert!(
                texts
                    .iter()
                    .any(|(text, _)| text == &series_label(name, "ohm"))
            );
        }
        output.drop_without_applying_deltas();
        assert_eq!(
            click_legend(&ctx, &mut view, &mut opts, "X", 5.0),
            ViewLock::Unchanged
        );
        assert_eq!(
            opts.series.iter().map(|s| s.visible).collect::<Vec<_>>(),
            [false, false, true]
        );
        let (_, paths) = frame(&ctx, &mut view, &opts, vec![], 5.2);
        assert_eq!(paths.len(), 1);
        assert_eq!(view, original_view);
        assert_eq!(
            opts.series
                .iter()
                .map(|s| s.points.clone())
                .collect::<Vec<_>>(),
            original_points
        );
    }

    #[test]
    fn double_clicking_legend_does_not_reset_or_lock_the_view() {
        let ctx = egui::Context::default();
        let mut opts = impedance_options();
        let original = PlotView::new(1e7, 1e8, -5.0, 5.0);
        let mut view = original;
        assert_eq!(
            click_legend(&ctx, &mut view, &mut opts, "R", 1.0),
            ViewLock::Unchanged
        );
        assert!(!opts.series[1].visible);
        assert_eq!(
            click_legend(&ctx, &mut view, &mut opts, "R", 1.1),
            ViewLock::Unchanged
        );
        assert!(opts.series[1].visible);
        assert_eq!(view, original);
    }

    #[test]
    fn wheel_over_legend_does_not_zoom_the_plot() {
        let ctx = egui::Context::default();
        let mut opts = impedance_options();
        let original = PlotView::new(1e6, 1e9, -100.0, 100.0);
        let mut view = original;
        let (_, output) = widget_frame(&ctx, &mut view, &mut opts, vec![], 0.0);
        let pos = text_shapes(&output)
            .into_iter()
            .find(|(text, _)| text == "R ohm")
            .unwrap()
            .1;
        output.drop_without_applying_deltas();
        let (_, output) = widget_frame(
            &ctx,
            &mut view,
            &mut opts,
            vec![egui::Event::PointerMoved(pos)],
            0.02,
        );
        output.drop_without_applying_deltas();
        let (lock, output) = widget_frame(
            &ctx,
            &mut view,
            &mut opts,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, 3.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            }],
            0.04,
        );
        assert_eq!(lock, ViewLock::Unchanged);
        assert_eq!(view, original);
        output.drop_without_applying_deltas();
    }

    #[test]
    fn hidden_series_are_omitted_from_cursor_readouts() {
        let ctx = egui::Context::default();
        let mut opts = impedance_options();
        opts.series[0].visible = false;
        opts.series[1].visible = false;
        let mut view = PlotView::new(1e6, 1e9, -100.0, 100.0);
        let (_, output) = widget_frame(&ctx, &mut view, &mut opts, vec![], 0.0);
        output.drop_without_applying_deltas();
        let (_, output) = widget_frame(
            &ctx,
            &mut view,
            &mut opts,
            vec![egui::Event::PointerMoved(Pos2::new(300.0, 200.0))],
            0.02,
        );
        let texts = text_shapes(&output);
        assert!(texts.iter().any(|(text, _)| text == "X -10 ohm"));
        assert!(
            !texts
                .iter()
                .any(|(text, _)| text == "|Z| 1000 ohm" || text == "R 20 ohm")
        );
        output.drop_without_applying_deltas();
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
