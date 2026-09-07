// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Smith chart, custom-drawn with `egui::Painter`.
//!
//! Input is a `z`-format S11 sweep: each point carries
//! `[|Z|, R, X]` in ohms (protocol doc 4.2/4.4). The reflection
//! coefficient is computed as gamma = (Z - Z0) / (Z + Z0) with Z0 = 50.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke};
use kcsdi_core::data::SweepData;

use crate::theme::TRACE_COLORS;

/// Nominal system impedance in ohms.
pub const Z0: f64 = 50.0;

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
/// Gap between the unit circle and the plot edge at zoom = 1.
const FIT_MARGIN: f32 = 0.95;

// Chart chrome colors from reference UI analysis section 4.3 (dark theme).
const BG_COLOR: Color32 = Color32::from_rgb(0x18, 0x18, 0x18);
const GRID_COLOR: Color32 = Color32::from_rgb(0x42, 0x42, 0x42);
const OUTLINE_COLOR: Color32 = Color32::from_rgb(0x5a, 0x5a, 0x5a);
const TEXT_COLOR: Color32 = Color32::from_rgb(0xd5, 0xd5, 0xd5);

/// (a + jb) / (c + jd).
fn cdiv(a: f64, b: f64, c: f64, d: f64) -> (f64, f64) {
    let m = c * c + d * d;
    ((a * c + b * d) / m, (b * c - a * d) / m)
}

/// Reflection coefficient of Z = r + jx ohms against `z0`:
/// gamma = (Z - z0) / (Z + z0).
fn gamma_of(r: f64, x: f64, z0: f64) -> (f64, f64) {
    cdiv(r - z0, x, r + z0, x)
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
            scale: rect.width().min(rect.height()) / 2.0 * FIT_MARGIN * view.zoom as f32,
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
    let (rect, response) = ui.allocate_at_least(ui.available_size(), Sense::click_and_drag());

    handle_input(ui, view, rect, &response);

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, BG_COLOR);
    if rect.width() < 2.0 || rect.height() < 2.0 {
        return;
    }

    let mapping = Mapping::new(rect, view);
    let clipped = painter.with_clip_rect(rect);
    draw_grid(&clipped, &mapping);

    let points = trace_points(trace);
    if points.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No data",
            FontId::monospace(14.0),
            TEXT_COLOR,
        );
    } else {
        draw_trace(&clipped, &mapping, &points);
        draw_hover(&clipped, &mapping, &points, rect, &response);
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
        let factor = if scroll > 0.0 { 1.1 } else { 1.0 / 1.1 };
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

/// (freq_hz, R, X, gamma_u, gamma_v) for every usable sweep point.
fn trace_points(trace: Option<&SweepData>) -> Vec<(f64, f64, f64, f64, f64)> {
    let mut out = Vec::new();
    let Some(data) = trace else {
        return out;
    };
    for p in &data.points {
        let [r, x] = match p.values.as_slice() {
            [_, r, x, ..] => [*r, *x],
            _ => continue,
        };
        if !p.freq_hz.is_finite() || !r.is_finite() || !x.is_finite() {
            continue;
        }
        let (u, v) = gamma_of(r, x, Z0);
        out.push((p.freq_hz, r, x, u, v));
    }
    out
}

fn draw_grid(painter: &egui::Painter, mapping: &Mapping) {
    let grid = Stroke::new(1.0, GRID_COLOR);

    // Unit circle, slightly brighter than the inner grid.
    painter.circle_stroke(
        mapping.to_screen(0.0, 0.0),
        mapping.scale,
        Stroke::new(1.0, OUTLINE_COLOR),
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

/// Gamma polyline of the sweep. Uncalibrated data can exceed |gamma| =
/// 1; such points are drawn as-is (the clip rect bounds them).
fn draw_trace(painter: &egui::Painter, mapping: &Mapping, points: &[(f64, f64, f64, f64, f64)]) {
    let stroke = Stroke::new(1.5, TRACE_COLORS[0]);
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
    response: &egui::Response,
) {
    let Some(pos) = response.hover_pos() else {
        return;
    };
    if !rect.contains(pos) {
        return;
    }
    let (gu, gv) = mapping.to_gamma(pos);
    let Some(&(freq, r, x, u, v)) = points.iter().min_by(|a, b| {
        let da = (a.3 - gu).powi(2) + (a.4 - gv).powi(2);
        let db = (b.3 - gu).powi(2) + (b.4 - gv).powi(2);
        da.total_cmp(&db)
    }) else {
        return;
    };

    let at = mapping.to_screen(u, v);
    painter.circle_stroke(at, 4.0, Stroke::new(1.0, TEXT_COLOR));
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        format!("{}  {:.2} {:+.2} j ohm", format_hz(freq), r, x),
        FontId::monospace(LABEL_FONT_SIZE),
        TEXT_COLOR,
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

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn cdiv_divides_complex_numbers() {
        // (1 + 2j) / (3 + 4j) = (11 + 2j) / 25
        let (re, im) = cdiv(1.0, 2.0, 3.0, 4.0);
        assert!(approx(re, 11.0 / 25.0));
        assert!(approx(im, 2.0 / 25.0));
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
        assert!(approx(u, 0.3));
        assert!(approx(v, -0.4));
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
                    values: vec![50.0], // too short, skipped
                },
                kcsdi_core::data::SweepPoint {
                    freq_hz: 3e6,
                    values: vec![10.0, 0.0, 50.0],
                },
            ],
        };
        let points = trace_points(Some(&data));
        assert_eq!(points.len(), 2);
        assert!(approx(points[0].3, 0.0) && approx(points[0].4, 0.0));
        assert!(approx(points[1].3, 0.0) && approx(points[1].4, 1.0));
        assert!(trace_points(None).is_empty());
    }

    #[test]
    fn format_hz_uses_si_prefix() {
        assert_eq!(format_hz(2.4e9), "2.40 GHz");
        assert_eq!(format_hz(433e6), "433.00 MHz");
        assert_eq!(format_hz(12e3), "12.00 kHz");
        assert_eq!(format_hz(900.0), "900.00 Hz");
    }
}
