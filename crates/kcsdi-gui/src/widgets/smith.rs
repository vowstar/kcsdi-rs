// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Smith chart, custom-drawn with `egui::Painter`.
//!
//! Input is a `z`-format S11 sweep: each point carries
//! `[|Z|, R, X]` in ohms (protocol doc 4.2/4.4). The reflection
//! coefficient is computed as gamma = (Z - Z0) / (Z + Z0) with Z0 = 50.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke};
use kcsdi_core::data::SweepData;

use super::plot::{Marker, chart_color, wheel_factor};
use crate::i18n::{Text, language};
use crate::theme::trace_colors;

/// Nominal system impedance in ohms.
pub const Z0: f64 = kcsdi_core::touchstone::REFERENCE_OHMS;

/// Viewport of the Smith chart: zoom around the center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmithView {
    /// Zoom factor, 1.0 = unit circle fills the plot.
    pub zoom: f64,
    /// Pan offset in gamma units.
    pub dx: f64,
    pub dy: f64,
}

impl Default for SmithView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            dx: 0.0,
            dy: 0.0,
        }
    }
}

impl SmithView {
    /// Reset to the default full-circle view.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Normalized resistance values of the grid circles.
const RESISTANCE_GRID: [f64; 5] = [0.2, 0.5, 1.0, 2.0, 5.0];
/// Normalized reactance values of the grid arcs (drawn for +/- each).
const REACTANCE_GRID: [f64; 5] = [0.2, 0.5, 1.0, 2.0, 5.0];
/// Polyline samples per reactance arc.
const ARC_SEGMENTS: usize = 256;
const LABEL_FONT_SIZE: f32 = 11.0;
const MIN_ZOOM: f64 = 0.2;
const MAX_ZOOM: f64 = 200.0;
/// Space for labels outside the unit circle at zoom = 1.
const GRID_LABEL_MARGIN: f32 = 36.0;
const HEADER_HEIGHT: f32 = 38.0;
const FOOTER_HEIGHT: f32 = 22.0;

// Chart chrome colors from reference UI analysis section 4.3 (dark theme).
const BG_COLOR: Color32 = Color32::from_rgb(0x18, 0x18, 0x18);
const GRID_COLOR: Color32 = Color32::from_rgb(0x42, 0x42, 0x42);
const OUTLINE_COLOR: Color32 = Color32::from_rgb(0x5a, 0x5a, 0x5a);
const TEXT_COLOR: Color32 = Color32::from_rgb(0xd5, 0xd5, 0xd5);

/// Reflection coefficient of Z = r + jx ohms against `z0`:
/// gamma = (Z - z0) / (Z + z0).
fn gamma_of(r: f64, x: f64, z0: f64) -> (f64, f64) {
    kcsdi_core::touchstone::reflection_coefficient(r, x, z0).unwrap_or((f64::NAN, f64::NAN))
}

/// Circle of constant normalized resistance `r`: center on the real
/// axis, passing through gamma = 1.
fn resistance_circle(r: f64) -> ((f64, f64), f64) {
    ((r / (r + 1.0), 0.0), 1.0 / (r + 1.0))
}

/// Circle of constant normalized reactance `x`: center on the vertical
/// line through gamma = 1, also passing through gamma = 1.
fn reactance_circle(x: f64) -> ((f64, f64), f64) {
    ((1.0, 1.0 / x), 1.0 / x.abs())
}

/// Sample the reactance circle, keeping only the arc inside the unit
/// circle (the part of the Smith chart that is drawn).
fn reactance_arc(x: f64, segments: usize) -> Vec<(f64, f64)> {
    let ((cx, cy), radius) = reactance_circle(x);
    let mut points = Vec::new();
    for i in 0..=segments {
        let t = i as f64 / segments as f64 * std::f64::consts::TAU;
        let (u, v) = (cx + radius * t.cos(), cy + radius * t.sin());
        if u * u + v * v <= 1.0 + 1e-9 {
            points.push((u, v));
        }
    }
    points
}

/// Affine map between gamma space and screen space for the current view.
struct Mapping {
    center: Pos2,
    /// Screen pixels per gamma unit.
    scale: f32,
    dx: f64,
    dy: f64,
}

impl Mapping {
    fn new(rect: Rect, view: &SmithView) -> Self {
        Self {
            center: rect.center(),
            scale: (rect.width().min(rect.height()) / 2.0 - GRID_LABEL_MARGIN).max(1.0)
                * view.zoom as f32,
            dx: view.dx,
            dy: view.dy,
        }
    }

    fn to_screen(&self, u: f64, v: f64) -> Pos2 {
        Pos2::new(
            self.center.x + ((u - self.dx) * self.scale as f64) as f32,
            self.center.y - ((v - self.dy) * self.scale as f64) as f32,
        )
    }

    fn to_gamma(&self, pos: Pos2) -> (f64, f64) {
        (
            self.dx + (pos.x - self.center.x) as f64 / self.scale as f64,
            self.dy - (pos.y - self.center.y) as f64 / self.scale as f64,
        )
    }
}

/// Draw the Smith chart into the available space. Signature is a module
/// contract; do not change it.
pub fn show(ui: &mut egui::Ui, view: &mut SmithView, trace: Option<&SweepData>) {
    show_with_markers(ui, view, trace, &mut []);
}

pub fn show_with_markers(
    ui: &mut egui::Ui,
    view: &mut SmithView,
    trace: Option<&SweepData>,
    markers: &mut [Marker],
) {
    show_layers(ui, view, trace, None, markers);
}

pub fn show_layers(
    ui: &mut egui::Ui,
    view: &mut SmithView,
    trace: Option<&SweepData>,
    held: Option<&SweepData>,
    markers: &mut [Marker],
) {
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, chart_color(painter.ctx(), BG_COLOR));
    let chart_rect = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + HEADER_HEIGHT),
        Pos2::new(rect.right(), rect.bottom() - FOOTER_HEIGHT),
    );
    if chart_rect.width() < 2.0 || chart_rect.height() < 2.0 {
        return;
    }

    let points = trace_points(trace);
    if !interact_markers(
        ui,
        &Mapping::new(chart_rect, view),
        chart_rect,
        &points,
        markers,
    ) {
        handle_input(ui, view, chart_rect, &response);
    }
    let mapping = Mapping::new(chart_rect, view);
    let clipped = painter.with_clip_rect(chart_rect);
    draw_grid(&clipped, &mapping);
    draw_grid_labels(&clipped, &mapping, chart_rect);
    for (row, text) in [
        format!("{} | Z0 = {Z0} ohm", language(ui.ctx()).text(Text::Smith)),
        "r = R/Z0   x = X/Z0".to_string(),
    ]
    .iter()
    .enumerate()
    {
        painter.text(
            Pos2::new(rect.left() + 4.0, rect.top() + 4.0 + row as f32 * 15.0),
            Align2::LEFT_TOP,
            text,
            FontId::monospace(LABEL_FONT_SIZE),
            chart_color(painter.ctx(), TEXT_COLOR),
        );
    }

    let held_points = trace_points(held);
    draw_trace_color(
        &clipped,
        &mapping,
        &held_points,
        Color32::from_rgb(0x28, 0x99, 0xd0),
    );
    if !points.iter().any(|p| p.3.is_finite() && p.4.is_finite()) {
        painter.text(
            Pos2::new(rect.left() + 4.0, rect.bottom() - 4.0),
            Align2::LEFT_BOTTOM,
            language(ui.ctx()).text(Text::NoData),
            FontId::monospace(14.0),
            chart_color(painter.ctx(), TEXT_COLOR),
        );
    } else {
        draw_trace(&clipped, &mapping, &points);
        draw_hover(painter, &mapping, &points, rect, chart_rect, &response);
    }
    draw_markers(&clipped, &mapping, &points, markers);
}

fn marker_point<'a>(
    points: &'a [(f64, f64, f64, f64, f64)],
    marker: &Marker,
) -> Option<&'a (f64, f64, f64, f64, f64)> {
    points
        .iter()
        .filter(|p| p.0.is_finite() && p.3.is_finite() && p.4.is_finite())
        .min_by(|a, b| {
            (a.0 - marker.frequency_hz)
                .abs()
                .total_cmp(&(b.0 - marker.frequency_hz).abs())
        })
}

fn interact_markers(
    ui: &egui::Ui,
    mapping: &Mapping,
    rect: Rect,
    points: &[(f64, f64, f64, f64, f64)],
    markers: &mut [Marker],
) -> bool {
    let mut selected = None;
    let mut busy = false;
    for marker in markers.iter_mut() {
        let Some(point) = marker_point(points, marker) else {
            continue;
        };
        let at = mapping.to_screen(point.3, point.4);
        if !rect.contains(at) {
            continue;
        }
        let response = ui
            .interact(
                Rect::from_center_size(at, egui::vec2(22.0, 22.0)),
                ui.id().with(("smith_marker", marker.id)),
                Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::Grab);
        busy |= response.hovered() || response.dragged();
        if response.clicked() || response.dragged() {
            selected = Some(marker.id);
        }
        if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let (u, v) = mapping.to_gamma(pos);
            if let Some(point) = points
                .iter()
                .filter(|p| p.3.is_finite() && p.4.is_finite())
                .min_by(|a, b| {
                    ((a.3 - u).powi(2) + (a.4 - v).powi(2))
                        .total_cmp(&((b.3 - u).powi(2) + (b.4 - v).powi(2)))
                })
            {
                marker.frequency_hz = point.0;
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

fn draw_markers(
    painter: &egui::Painter,
    mapping: &Mapping,
    points: &[(f64, f64, f64, f64, f64)],
    markers: &[Marker],
) {
    for marker in markers {
        let Some(point) = marker_point(points, marker) else {
            continue;
        };
        let at = mapping.to_screen(point.3, point.4);
        let color = if marker.selected {
            painter.ctx().global_style().visuals.selection.stroke.color
        } else {
            chart_color(painter.ctx(), TEXT_COLOR)
        };
        painter.circle_stroke(at, 5.0, Stroke::new(1.5, color));
        painter.text(
            at + egui::vec2(7.0, -7.0),
            Align2::LEFT_BOTTOM,
            format!("M{}", marker.id),
            FontId::monospace(12.0),
            color,
        );
    }
}

/// Wheel zoom anchored at the cursor, drag pan, double-click reset,
/// per reference UI analysis section 5. A held Shift makes some
/// platforms report the wheel as horizontal scroll, so the dominant
/// delta component is used in that case.
fn handle_input(ui: &egui::Ui, view: &mut SmithView, rect: Rect, response: &egui::Response) {
    let (delta, modifiers) = ui.ctx().input(|i| (i.smooth_scroll_delta, i.modifiers));
    let scroll = if modifiers.shift {
        if delta.x.abs() > delta.y.abs() {
            delta.x
        } else {
            delta.y
        }
    } else {
        delta.y
    };
    if scroll != 0.0
        && response.hovered()
        && let Some(pos) = response.hover_pos()
        && rect.contains(pos)
    {
        let factor = wheel_factor(scroll).recip();
        let new_zoom = (view.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if new_zoom != view.zoom {
            // Keep the gamma under the cursor fixed: solve for the pan
            // offset that maps it back to the same screen point.
            let before = Mapping::new(rect, view);
            let (gu, gv) = before.to_gamma(pos);
            view.zoom = new_zoom;
            let after = Mapping::new(rect, view);
            let s = after.scale as f64;
            view.dx = gu - (pos.x - after.center.x) as f64 / s;
            view.dy = gv + (pos.y - after.center.y) as f64 / s;
        }
    }

    if response.dragged() {
        let d = response.drag_delta();
        let mapping = Mapping::new(rect, view);
        let s = mapping.scale as f64;
        if s > 0.0 {
            view.dx -= d.x as f64 / s;
            view.dy += d.y as f64 / s;
        }
    }

    if response.double_clicked() {
        view.reset();
    }
}

/// (freq_hz, R, X, gamma_u, gamma_v) for each sweep point. Invalid
/// samples retain a non-finite gamma so the trace keeps its gaps.
fn trace_points(trace: Option<&SweepData>) -> Vec<(f64, f64, f64, f64, f64)> {
    let mut out = Vec::new();
    let Some(data) = trace else {
        return out;
    };
    if data.mode != kcsdi_core::protocol::StreamMode::S11 || data.format != "z" {
        return out;
    }
    for p in &data.points {
        let r = p.values.get(1).copied().unwrap_or(f64::NAN);
        let x = p.values.get(2).copied().unwrap_or(f64::NAN);
        let (u, v) = if p.freq_hz.is_finite() && r.is_finite() && x.is_finite() {
            gamma_of(r, x, Z0)
        } else {
            (f64::NAN, f64::NAN)
        };
        out.push((p.freq_hz, r, x, u, v));
    }
    out
}

fn draw_grid(painter: &egui::Painter, mapping: &Mapping) {
    let grid = Stroke::new(1.0, chart_color(painter.ctx(), GRID_COLOR));

    // Unit circle, slightly brighter than the inner grid.
    painter.circle_stroke(
        mapping.to_screen(0.0, 0.0),
        mapping.scale,
        Stroke::new(1.0, chart_color(painter.ctx(), OUTLINE_COLOR)),
    );

    for &r in &RESISTANCE_GRID {
        let ((cu, cv), radius) = resistance_circle(r);
        painter.circle_stroke(
            mapping.to_screen(cu, cv),
            radius as f32 * mapping.scale,
            grid,
        );
    }

    for &x in &REACTANCE_GRID {
        for sign in [1.0, -1.0] {
            let arc = reactance_arc(x * sign, ARC_SEGMENTS);
            let screen: Vec<Pos2> = arc.iter().map(|&(u, v)| mapping.to_screen(u, v)).collect();
            if screen.len() >= 2 {
                painter.add(Shape::line(screen, grid));
            }
        }
    }

    // Real axis.
    painter.line_segment(
        [mapping.to_screen(-1.0, 0.0), mapping.to_screen(1.0, 0.0)],
        grid,
    );
}

/// Normalized resistance labels sit on the real axis. Reactance labels
/// sit at the unit-circle ends of their arcs, positive above the axis.
/// Reference loads and region captions share the same collision check.
fn draw_grid_labels(painter: &egui::Painter, mapping: &Mapping, rect: Rect) {
    let language = language(painter.ctx());
    let font = FontId::monospace(LABEL_FONT_SIZE);
    let mut occupied: Vec<Rect> = Vec::new();
    let mut label = |text: String, at: Pos2, align: Align2| {
        let galley =
            painter.layout_no_wrap(text, font.clone(), chart_color(painter.ctx(), TEXT_COLOR));
        let bounds = align.anchor_size(at, galley.size());
        // Never pin an off-screen label to the edge, where it would no
        // longer identify its grid line. Suppress collisions when zoomed out.
        if rect.contains_rect(bounds)
            && occupied
                .iter()
                .all(|other| !other.expand(2.0).intersects(bounds))
        {
            painter.rect_filled(
                bounds.expand(1.0),
                0.0,
                chart_color(painter.ctx(), BG_COLOR),
            );
            painter.galley(bounds.min, galley, chart_color(painter.ctx(), TEXT_COLOR));
            occupied.push(bounds);
        }
    };

    // Keep the matched load readable before adding the other labels.
    label(
        "1".to_string(),
        mapping.to_screen(0.0, 0.0) + egui::vec2(0.0, -3.0),
        Align2::CENTER_BOTTOM,
    );
    for (u, text, align) in [
        (0.0, language.text(Text::Match), Align2::CENTER_TOP),
        (-1.0, language.text(Text::Short), Align2::LEFT_TOP),
        (1.0, language.text(Text::Open), Align2::RIGHT_TOP),
    ] {
        let at = mapping.to_screen(u, 0.0);
        if rect.shrink(4.0).contains(at) {
            painter.circle_stroke(
                at,
                3.0,
                Stroke::new(1.0, chart_color(painter.ctx(), TEXT_COLOR)),
            );
            label(
                text.to_string(),
                at + egui::vec2(-u as f32 * 6.0, 6.0),
                align,
            );
        }
    }
    for r in RESISTANCE_GRID.into_iter().filter(|r| *r != 1.0) {
        let (u, v) = gamma_of(r, 0.0, 1.0);
        label(
            r.to_string(),
            mapping.to_screen(u, v) + egui::vec2(0.0, -3.0),
            Align2::CENTER_BOTTOM,
        );
    }
    label(
        "0".to_string(),
        mapping.to_screen(-1.0, 0.0) + egui::vec2(-4.0, -3.0),
        Align2::RIGHT_BOTTOM,
    );
    label(
        "inf".to_string(),
        mapping.to_screen(1.0, 0.0) + egui::vec2(4.0, -3.0),
        Align2::LEFT_BOTTOM,
    );

    for x in REACTANCE_GRID {
        for sign in [1.0, -1.0] {
            let (u, v) = gamma_of(0.0, sign * x, 1.0);
            let at = mapping.to_screen(u, v) + egui::vec2(u as f32 * 5.0, -v as f32 * 5.0);
            let align = Align2([
                if u < -0.01 {
                    egui::Align::RIGHT
                } else if u > 0.01 {
                    egui::Align::LEFT
                } else {
                    egui::Align::Center
                },
                if v > 0.0 {
                    egui::Align::BOTTOM
                } else {
                    egui::Align::TOP
                },
            ]);
            label(
                format!("{}j{x}", if sign > 0.0 { "+" } else { "-" }),
                at,
                align,
            );
        }
    }

    // These describe regions, not extra grid lines. Omit them when the
    // circle is too small to leave the trace and numeric labels readable.
    if mapping.scale >= 100.0 {
        for (v, text) in [
            (0.7, language.text(Text::Inductive)),
            (-0.7, language.text(Text::Capacitive)),
        ] {
            label(
                text.to_string(),
                mapping.to_screen(-0.35, v),
                Align2::CENTER_CENTER,
            );
        }
    }
}

/// Gamma polyline of the sweep. Uncalibrated data can exceed |gamma| =
/// 1; such points are drawn as-is (the clip rect bounds them).
fn draw_trace(painter: &egui::Painter, mapping: &Mapping, points: &[(f64, f64, f64, f64, f64)]) {
    let color = trace_colors(painter.ctx().global_style().visuals.dark_mode)[0];
    draw_trace_color(painter, mapping, points, color);
}

fn draw_trace_color(
    painter: &egui::Painter,
    mapping: &Mapping,
    points: &[(f64, f64, f64, f64, f64)],
    color: Color32,
) {
    let stroke = Stroke::new(1.5, color);
    let mut run: Vec<Pos2> = Vec::new();
    for &(_, _, _, u, v) in points {
        if u.is_finite() && v.is_finite() {
            run.push(mapping.to_screen(u, v));
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

/// Nearest-point marker plus an R + jX / frequency readout while the
/// cursor is over the chart.
fn draw_hover(
    painter: &egui::Painter,
    mapping: &Mapping,
    points: &[(f64, f64, f64, f64, f64)],
    rect: Rect,
    chart_rect: Rect,
    response: &egui::Response,
) {
    let Some(pos) = response.hover_pos() else {
        return;
    };
    if !chart_rect.contains(pos) {
        return;
    }
    let (gu, gv) = mapping.to_gamma(pos);
    let Some(&(freq, r, x, u, v)) = points
        .iter()
        .filter(|p| p.3.is_finite() && p.4.is_finite())
        .min_by(|a, b| {
            let da = (a.3 - gu).powi(2) + (a.4 - gv).powi(2);
            let db = (b.3 - gu).powi(2) + (b.4 - gv).powi(2);
            da.total_cmp(&db)
        })
    else {
        return;
    };

    let at = mapping.to_screen(u, v);
    painter.with_clip_rect(chart_rect).circle_stroke(
        at,
        4.0,
        Stroke::new(1.0, chart_color(painter.ctx(), TEXT_COLOR)),
    );
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        format!("{}  R = {:.2} ohm  X = {:+.2} ohm", format_hz(freq), r, x),
        FontId::monospace(LABEL_FONT_SIZE),
        chart_color(painter.ctx(), TEXT_COLOR),
    );
}

/// Frequency with an SI prefix, e.g. "433.00 MHz".
fn format_hz(hz: f64) -> String {
    let (unit, scale) = if hz.abs() >= 1e9 {
        ("GHz", 1e9)
    } else if hz.abs() >= 1e6 {
        ("MHz", 1e6)
    } else if hz.abs() >= 1e3 {
        ("kHz", 1e3)
    } else {
        ("Hz", 1.0)
    };
    format!("{:.2} {}", hz / scale, unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_zoom_stays_moderate_after_the_smoothing_tail() {
        let ctx = egui::Context::default();
        let mut view = SmithView::default();
        for index in 0..100 {
            let events = match index {
                1 => vec![egui::Event::PointerMoved(Pos2::new(300.0, 300.0))],
                2 => vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 3.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                }],
                _ => vec![],
            };
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0))),
                    time: Some(index as f64 / 60.0),
                    events,
                    ..Default::default()
                },
                |ui| show(ui, &mut view, None),
            )
            .drop_without_applying_deltas();
        }
        assert!(
            (1.01..1.5).contains(&view.zoom),
            "zoom after smoothing: {}",
            view.zoom
        );
    }

    #[test]
    fn dragging_a_smith_marker_follows_measured_points_without_panning() {
        let ctx = egui::Context::default();
        let mut view = SmithView::default();
        let data = SweepData {
            mode: kcsdi_core::protocol::StreamMode::S11,
            format: "z".into(),
            points: [50.0, 100.0, 150.0]
                .into_iter()
                .enumerate()
                .map(|(index, r)| kcsdi_core::data::SweepPoint {
                    freq_hz: (index + 1) as f64 * 1e6,
                    values: vec![r, r, 0.0],
                })
                .collect(),
        };
        let chart = Rect::from_min_max(
            Pos2::new(0.0, HEADER_HEIGHT),
            Pos2::new(600.0, 600.0 - FOOTER_HEIGHT),
        );
        let mapping = Mapping::new(chart, &view);
        let from = mapping.to_screen(0.0, 0.0);
        let to = mapping.to_screen(0.5, 0.0);
        let mut markers = vec![Marker {
            id: 1,
            frequency_hz: 1e6,
            selected: true,
            reference: false,
        }];
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
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0))),
                    time: Some(index as f64 / 60.0),
                    events,
                    ..Default::default()
                },
                |ui| show_with_markers(ui, &mut view, Some(&data), &mut markers),
            )
            .drop_without_applying_deltas();
        }
        assert_eq!(markers[0].frequency_hz, 3e6);
        assert_eq!(view, SmithView::default());
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn gamma_conversion_handles_large_values_and_rejects_singularities() {
        let (re, im) = gamma_of(1e300, 1e300, Z0);
        assert!(approx(re, 1.0));
        assert!(im.is_finite());
        let (re, im) = gamma_of(-Z0, 0.0, Z0);
        assert!(re.is_nan() && im.is_nan());
    }

    #[test]
    fn gamma_of_matched_load_is_zero() {
        let (u, v) = gamma_of(Z0, 0.0, Z0);
        assert!(approx(u, 0.0));
        assert!(approx(v, 0.0));
    }

    #[test]
    fn gamma_of_short_is_minus_one() {
        let (u, v) = gamma_of(0.0, 0.0, Z0);
        assert!(approx(u, -1.0));
        assert!(approx(v, 0.0));
    }

    #[test]
    fn gamma_of_open_approaches_plus_one() {
        let (u, v) = gamma_of(1e12, 0.0, Z0);
        assert!(u > 0.9999);
        assert!(approx(v, 0.0));
    }

    #[test]
    fn gamma_of_pure_reactance_sits_on_unit_circle() {
        let (u, v) = gamma_of(0.0, Z0, Z0);
        assert!(approx(u.hypot(v), 1.0));
        // Series formula: (jx - z0)/(jx + z0) for x = z0 gives j.
        assert!(approx(u, 0.0));
        assert!(approx(v, 1.0));
    }

    #[test]
    fn resistance_circle_geometry() {
        let ((u, v), radius) = resistance_circle(1.0);
        assert!(approx(u, 0.5) && approx(v, 0.0) && approx(radius, 0.5));
        let ((u, _), radius) = resistance_circle(0.0);
        assert!(approx(u, 0.0) && approx(radius, 1.0));
        // Every resistance circle passes through gamma = 1.
        for &r in &RESISTANCE_GRID {
            let ((cu, cv), radius) = resistance_circle(r);
            assert!(approx((1.0 - cu).hypot(cv), radius));
        }
    }

    #[test]
    fn reactance_circle_geometry() {
        let ((u, v), radius) = reactance_circle(1.0);
        assert!(approx(u, 1.0) && approx(v, 1.0) && approx(radius, 1.0));
        let ((_, v), radius) = reactance_circle(-2.0);
        assert!(approx(v, -0.5) && approx(radius, 0.5));
        // Every reactance circle passes through gamma = 1.
        for &x in &REACTANCE_GRID {
            let ((cu, cv), radius) = reactance_circle(x);
            assert!(approx((1.0 - cu).hypot(0.0 - cv), radius));
        }
    }

    #[test]
    fn reactance_arc_stays_inside_unit_circle() {
        for &x in &REACTANCE_GRID {
            for sign in [1.0, -1.0] {
                let arc = reactance_arc(x * sign, ARC_SEGMENTS);
                assert!(!arc.is_empty());
                for &(u, v) in &arc {
                    assert!(u * u + v * v <= 1.0 + 1e-9);
                }
            }
        }
    }

    #[test]
    fn mapping_roundtrips() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), egui::vec2(200.0, 100.0));
        let view = SmithView {
            zoom: 2.0,
            dx: 0.1,
            dy: -0.2,
        };
        let mapping = Mapping::new(rect, &view);
        let pos = mapping.to_screen(0.3, -0.4);
        let (u, v) = mapping.to_gamma(pos);
        // Screen coordinates are f32, unlike the impedance calculations.
        assert!((u - 0.3).abs() < 1e-6);
        assert!((v + 0.4).abs() < 1e-6);
    }

    fn text_shapes(output: &egui::FullOutput) -> Vec<(String, Rect)> {
        output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::Text(text) => Some((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                _ => None,
            })
            .collect()
    }

    fn rendered_labels(rect: Rect, view: SmithView) -> Vec<(String, Rect)> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            },
            |ui| {
                draw_grid_labels(ui.painter(), &Mapping::new(rect, &view), rect);
            },
        );
        let labels = text_shapes(&output);
        output.drop_without_applying_deltas();
        labels
    }

    #[test]
    fn grid_labels_identify_normalized_resistance_and_signed_reactance() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let mapping = Mapping::new(rect, &SmithView::default());
        let labels = rendered_labels(rect, SmithView::default());
        assert_eq!(
            labels.len(),
            RESISTANCE_GRID.len() + 2 * REACTANCE_GRID.len() + 7
        );
        for r in RESISTANCE_GRID {
            let (_, bounds) = labels
                .iter()
                .find(|(text, _)| text == &r.to_string())
                .unwrap();
            let (u, v) = gamma_of(r, 0.0, 1.0);
            let at = mapping.to_screen(u, v);
            assert!((bounds.center().x - at.x).abs() < 1e-4);
            assert!((bounds.bottom() - (at.y - 3.0)).abs() < 1e-4);
        }
        for x in REACTANCE_GRID {
            for (prefix, sign) in [("+", 1.0), ("-", -1.0)] {
                let (_, bounds) = labels
                    .iter()
                    .find(|(text, _)| text == &format!("{prefix}j{x}"))
                    .unwrap();
                let (u, v) = gamma_of(0.0, sign * x, 1.0);
                let at = mapping.to_screen(u, v);
                if sign > 0.0 {
                    assert!(bounds.bottom() < at.y && at.y < rect.center().y);
                } else {
                    assert!(bounds.top() > at.y && at.y > rect.center().y);
                }
            }
        }
        assert!(labels.iter().any(|(text, _)| text == "0"));
        assert!(labels.iter().any(|(text, _)| text == "inf"));
    }

    #[test]
    fn reference_load_labels_follow_their_gamma_positions() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        for view in [
            SmithView::default(),
            SmithView {
                zoom: 0.75,
                dx: 0.1,
                dy: -0.2,
            },
        ] {
            let mapping = Mapping::new(rect, &view);
            let labels = rendered_labels(rect, view);
            for (u, text) in [(-1.0, "SHORT"), (0.0, "MATCH"), (1.0, "OPEN")] {
                let (_, bounds) = labels.iter().find(|(label, _)| label == text).unwrap();
                let at = mapping.to_screen(u, 0.0);
                assert!((bounds.top() - at.y - 6.0).abs() < 1e-4);
                let label_x = match text {
                    "SHORT" => bounds.left() - 6.0,
                    "OPEN" => bounds.right() + 6.0,
                    _ => bounds.center().x,
                };
                assert!((label_x - at.x).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn region_labels_follow_reactance_sign_and_hide_when_crowded() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        for view in [
            SmithView::default(),
            SmithView {
                zoom: 1.2,
                dx: 0.05,
                dy: 0.05,
            },
        ] {
            let mapping = Mapping::new(rect, &view);
            let labels = rendered_labels(rect, view);
            let (_, upper) = labels
                .iter()
                .find(|(text, _)| text == "Inductive (+X)")
                .unwrap();
            let (_, lower) = labels
                .iter()
                .find(|(text, _)| text == "Capacitive (-X)")
                .unwrap();
            let axis_y = mapping.to_screen(0.0, 0.0).y;
            assert!(upper.bottom() < axis_y);
            assert!(lower.top() > axis_y);
            assert!((upper.center() - mapping.to_screen(-0.35, 0.7)).length() < 1e-4);
            assert!((lower.center() - mapping.to_screen(-0.35, -0.7)).length() < 1e-4);
        }
        let labels = rendered_labels(
            rect,
            SmithView {
                zoom: MIN_ZOOM,
                ..Default::default()
            },
        );
        assert!(
            !labels
                .iter()
                .any(|(text, _)| text.contains("Inductive") || text.contains("Capacitive"))
        );
    }

    #[test]
    fn reference_markers_use_ideal_short_match_and_open_coordinates() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let mapping = Mapping::new(rect, &SmithView::default());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            },
            |ui| {
                draw_grid_labels(ui.painter(), &mapping, rect);
            },
        );
        let markers: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::Circle(circle) => Some(circle),
                _ => None,
            })
            .collect();
        assert_eq!(markers.len(), 3);
        for u in [-1.0, 0.0, 1.0] {
            let marker = markers
                .iter()
                .find(|marker| marker.center == mapping.to_screen(u, 0.0))
                .unwrap();
            assert_eq!(marker.radius, 3.0);
            assert_eq!(marker.stroke.color, TEXT_COLOR);
        }
        output.drop_without_applying_deltas();
    }

    #[test]
    fn grid_labels_follow_pan_and_zoom_without_growing_the_font() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let original = rendered_labels(rect, SmithView::default());
        let bounds_of = |labels: &[(String, Rect)], key: &str| {
            labels.iter().find(|(text, _)| text == key).unwrap().1
        };
        let before = bounds_of(&original, "1");
        let view = SmithView {
            dx: 0.1,
            dy: -0.2,
            ..Default::default()
        };
        let panned = rendered_labels(rect, view);
        let after = bounds_of(&panned, "1");
        let scale = Mapping::new(rect, &view).scale;
        assert!((after.center().x - before.center().x + 0.1 * scale).abs() < 1e-4);
        assert!((after.center().y - before.center().y + 0.2 * scale).abs() < 1e-4);
        let zoomed = rendered_labels(
            rect,
            SmithView {
                zoom: 2.0,
                ..Default::default()
            },
        );
        assert_eq!(bounds_of(&zoomed, "1").size(), before.size());
        assert!(
            !zoomed
                .iter()
                .any(|(text, _)| text == "+j1" || text == "-j1")
        );
    }

    #[test]
    fn crowded_grid_labels_stay_inside_the_chart_without_overlaps() {
        for size in [
            egui::vec2(600.0, 600.0),
            egui::vec2(220.0, 180.0),
            egui::vec2(80.0, 60.0),
        ] {
            for zoom in [MIN_ZOOM, 1.0, 2.0, MAX_ZOOM] {
                let rect = Rect::from_min_size(Pos2::ZERO, size);
                let labels = rendered_labels(
                    rect,
                    SmithView {
                        zoom,
                        ..Default::default()
                    },
                );
                for (i, (_, bounds)) in labels.iter().enumerate() {
                    assert!(rect.contains_rect(*bounds));
                    assert!(
                        labels[i + 1..]
                            .iter()
                            .all(|(_, other)| !bounds.intersects(*other))
                    );
                }
            }
        }
    }

    #[test]
    fn chart_shows_reference_and_normalization_without_a_sweep() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            },
            |ui| {
                show(ui, &mut SmithView::default(), None);
            },
        );
        let labels = text_shapes(&output);
        assert!(labels.iter().any(|(text, _)| text == "Smith | Z0 = 50 ohm"));
        assert!(labels.iter().any(|(text, _)| text == "r = R/Z0   x = X/Z0"));
        assert!(labels.iter().any(|(text, _)| text == "+j1"));
        assert!(labels.iter().any(|(text, _)| text == "-j1"));
        for expected in [
            "SHORT",
            "MATCH",
            "OPEN",
            "Inductive (+X)",
            "Capacitive (-X)",
        ] {
            assert!(labels.iter().any(|(text, _)| text == expected));
        }
        let (_, no_data) = labels.iter().find(|(text, _)| text == "No data").unwrap();
        assert!(no_data.top() >= rect.bottom() - FOOTER_HEIGHT);
        output.drop_without_applying_deltas();
    }

    #[test]
    fn language_switch_updates_annotations_but_preserves_math_and_units() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        for selected in crate::i18n::Language::ALL {
            crate::i18n::set_language(&ctx, selected);
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0))),
                    ..Default::default()
                },
                |ui| show(ui, &mut SmithView::default(), None),
            );
            let labels = text_shapes(&output);
            for key in [
                Text::Match,
                Text::Short,
                Text::Open,
                Text::Inductive,
                Text::Capacitive,
                Text::NoData,
            ] {
                assert!(
                    labels.iter().any(|(text, _)| text == selected.text(key)),
                    "{selected:?}: {key:?}"
                );
            }
            assert!(
                labels
                    .iter()
                    .any(|(text, _)| text
                        == &format!("{} | Z0 = 50 ohm", selected.text(Text::Smith)))
            );
            for expected in ["r = R/Z0   x = X/Z0", "+j1", "-j1"] {
                assert!(labels.iter().any(|(text, _)| text == expected));
            }
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn hover_readout_uses_actual_ohms_below_the_grid() {
        let data = SweepData {
            mode: kcsdi_core::protocol::StreamMode::S11,
            format: "z".to_string(),
            points: vec![kcsdi_core::data::SweepPoint {
                freq_hz: 433e6,
                values: vec![25f64.hypot(-10.0), 25.0, -10.0],
            }],
        };
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
        for events in [vec![], vec![egui::Event::PointerMoved(rect.center())]] {
            let hovering = !events.is_empty();
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                },
                |ui| {
                    show(ui, &mut SmithView::default(), Some(&data));
                },
            );
            if hovering {
                let labels = text_shapes(&output);
                let (_, readout) = labels
                    .iter()
                    .find(|(text, _)| text == "433.00 MHz  R = 25.00 ohm  X = -10.00 ohm")
                    .unwrap();
                assert!(readout.top() >= rect.bottom() - FOOTER_HEIGHT);
                let (_, header) = labels
                    .iter()
                    .find(|(text, _)| text.starts_with("Smith |"))
                    .unwrap();
                assert!(header.bottom() < HEADER_HEIGHT);
            }
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn trace_points_extracts_r_and_x_columns() {
        let data = SweepData {
            mode: kcsdi_core::protocol::StreamMode::S11,
            format: "z".to_string(),
            points: vec![
                kcsdi_core::data::SweepPoint {
                    freq_hz: 1e6,
                    values: vec![50.0, 50.0, 0.0],
                },
                kcsdi_core::data::SweepPoint {
                    freq_hz: 2e6,
                    values: vec![50.0], // too short, keeps a gap
                },
                kcsdi_core::data::SweepPoint {
                    freq_hz: 3e6,
                    values: vec![10.0, 0.0, 50.0],
                },
            ],
        };
        let points = trace_points(Some(&data));
        assert_eq!(points.len(), 3);
        assert!(approx(points[0].3, 0.0) && approx(points[0].4, 0.0));
        assert!(points[1].3.is_nan() && points[1].4.is_nan());
        assert!(approx(points[2].3, 0.0) && approx(points[2].4, 1.0));
        assert!(trace_points(None).is_empty());
        let mut wrong_format = data;
        wrong_format.format = "ma".to_string();
        assert!(trace_points(Some(&wrong_format)).is_empty());
    }

    #[test]
    fn invalid_smith_samples_break_paths_instead_of_joining_neighbors() {
        use kcsdi_core::data::SweepPoint;
        for invalid in [
            SweepPoint {
                freq_hz: 3e6,
                values: vec![50.0],
            },
            SweepPoint {
                freq_hz: 3e6,
                values: vec![50.0, f64::NAN, 0.0],
            },
            SweepPoint {
                freq_hz: f64::NAN,
                values: vec![50.0, 50.0, 0.0],
            },
            SweepPoint {
                freq_hz: 3e6,
                values: vec![50.0, -50.0, 0.0],
            },
        ] {
            let data = SweepData {
                mode: kcsdi_core::protocol::StreamMode::S11,
                format: "z".to_string(),
                points: vec![
                    SweepPoint {
                        freq_hz: 1e6,
                        values: vec![50.0, 50.0, -10.0],
                    },
                    SweepPoint {
                        freq_hz: 2e6,
                        values: vec![50.0, 50.0, 0.0],
                    },
                    invalid,
                    SweepPoint {
                        freq_hz: 4e6,
                        values: vec![50.0, 50.0, 10.0],
                    },
                    SweepPoint {
                        freq_hz: 5e6,
                        values: vec![50.0, 50.0, 20.0],
                    },
                ],
            };
            let points = trace_points(Some(&data));
            assert_eq!(points.len(), 5);
            assert!(!points[2].3.is_finite() && !points[2].4.is_finite());
            let ctx = egui::Context::default();
            let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 600.0));
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                },
                |ui| {
                    let mapping = Mapping::new(rect, &SmithView::default());
                    draw_trace(ui.painter(), &mapping, &points);
                },
            );
            let paths: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    Shape::Path(path) => Some(path),
                    _ => None,
                })
                .collect();
            assert_eq!(paths.len(), 2);
            assert!(paths.iter().all(|path| path.points.len() == 2));
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn format_hz_uses_si_prefix() {
        assert_eq!(format_hz(2.4e9), "2.40 GHz");
        assert_eq!(format_hz(433e6), "433.00 MHz");
        assert_eq!(format_hz(12e3), "12.00 kHz");
        assert_eq!(format_hz(900.0), "900.00 Hz");
    }
}
