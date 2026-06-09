use std::sync::{Arc, Mutex};

use egui::{Color32, Stroke};
use routers_realtime::context::MatchOutcome;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

use crate::monitor::store::VehicleTraceStore;

pub struct TracePlugin {
    store: Arc<Mutex<VehicleTraceStore>>,
}

impl TracePlugin {
    pub fn new(store: Arc<Mutex<VehicleTraceStore>>) -> Self {
        Self { store }
    }
}

/// Deterministic per-vehicle colour from vehicle ID bytes.
fn vehicle_colour(id: &str) -> Color32 {
    let hash = id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    hsl_to_rgb((hash % 32) as f32 * (360.0 / 32.0), 0.75, 0.55)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h as u32) / 60 {
        0 => (c, x, 0.0_f32),
        1 => (x, c, 0.0_f32),
        2 => (0.0_f32, c, x),
        3 => (0.0_f32, x, c),
        4 => (x, 0.0_f32, c),
        _ => (c, 0.0_f32, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

impl Plugin for TracePlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let Ok(store) = self.store.lock() else { return };
        let painter = ui.painter();

        // Viewport bounds for culling.
        let tl = projector.unproject(response.rect.left_top().to_vec2());
        let br = projector.unproject(response.rect.right_bottom().to_vec2());
        let lon_range = tl.x().min(br.x())..=tl.x().max(br.x());
        let lat_range = tl.y().min(br.y())..=tl.y().max(br.y());

        for (vehicle_id, fixes) in &store.traces {
            let Some(last) = fixes.back() else { continue };
            if !lon_range.contains(&last.raw_coord.x())
                || !lat_range.contains(&last.raw_coord.y())
            {
                continue;
            }

            let colour = vehicle_colour(vehicle_id);

            // ── Interpolated road route (previous fix → current) ────────────
            if let Some((polyline, _)) = store.routes.get(vehicle_id) {
                // Find the second-to-last matched fix to use as the trail start.
                // Locate the closest point in the polyline to that position, then
                // draw only the suffix — one GPS interval's worth of road geometry.
                let tail_start = fixes
                    .iter()
                    .rev()
                    .skip(2)
                    .find_map(|f| f.matched_coord);

                let start_idx = tail_start
                    .and_then(|anchor| {
                        polyline
                            .iter()
                            .enumerate()
                            .min_by(|(_, a), (_, b)| {
                                let da = (a.x - anchor.x()) * (a.x - anchor.x())
                                    + (a.y - anchor.y()) * (a.y - anchor.y());
                                let db = (b.x - anchor.x()) * (b.x - anchor.x())
                                    + (b.y - anchor.y()) * (b.y - anchor.y());
                                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(i, _)| i)
                    })
                    .unwrap_or(0);

                let tail = &polyline[start_idx..];
                if tail.len() >= 2 {
                    let screen_pts: Vec<_> = tail
                        .iter()
                        .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
                        .collect();
                    painter.line(
                        screen_pts,
                        Stroke::new(3.5, Color32::from_rgba_unmultiplied(
                            colour.r(), colour.g(), colour.b(), 210,
                        )),
                    );
                }
            }

            // ── Raw GPS dots + snap lines ────────────────────────────────────
            // Draw raw GPS positions as small grey dots and snap lines to their
            // matched counterparts. No raw polyline — the interpolated route
            // above already shows the road path without historical accumulation.
            for fix in fixes.iter() {
                let raw_screen = projector
                    .project(lon_lat(fix.raw_coord.x(), fix.raw_coord.y()))
                    .to_pos2();

                // Faint snap line to matched position
                if let Some(matched) = fix.matched_coord {
                    let matched_screen = projector
                        .project(lon_lat(matched.x(), matched.y()))
                        .to_pos2();
                    painter.line(
                        vec![raw_screen, matched_screen],
                        Stroke::new(0.5, Color32::from_rgba_unmultiplied(120, 120, 120, 50)),
                    );
                }
            }

            // ── Latest raw fix: outcome-tinted open circle ───────────────────
            {
                let last_raw = projector
                    .project(lon_lat(last.raw_coord.x(), last.raw_coord.y()))
                    .to_pos2();
                let dot_colour = match last.outcome {
                    MatchOutcome::Success => Color32::from_rgba_unmultiplied(160, 160, 160, 200),
                    MatchOutcome::NoCandidate => Color32::from_rgba_unmultiplied(220, 140, 50, 220),
                    MatchOutcome::Error => Color32::from_rgba_unmultiplied(220, 60, 60, 220),
                };
                painter.circle_stroke(last_raw, 3.0, Stroke::new(1.5, dot_colour));
            }

            // ── Latest matched fix: filled dot in vehicle colour ─────────────
            if let Some(last_matched) = fixes.iter().rev().find_map(|f| f.matched_coord) {
                let pos = projector
                    .project(lon_lat(last_matched.x(), last_matched.y()))
                    .to_pos2();
                painter.circle_filled(pos, 4.5, colour);
            }
        }
    }
}
