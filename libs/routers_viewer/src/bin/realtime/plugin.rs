use core::hash::{Hash, Hasher};
use std::hash::DefaultHasher;

use egui::{Color32, Stroke, ecolor::Hsva};
use geo::Point;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

/// A per-frame snapshot of one vehicle's trace, ready to render.
pub struct TraceLine {
    pub colour: Color32,
    pub points: Vec<Point>,
}

/// Draws each vehicle trace as a polyline in the vehicle's colour, with a
/// filled dot marking the most recent position.
pub struct TracesPlugin {
    traces: Vec<TraceLine>,
}

impl TracesPlugin {
    pub fn new(traces: Vec<TraceLine>) -> Self {
        Self { traces }
    }
}

/// Stable, well-spread colour for a vehicle id: hash to a hue, keep
/// saturation and value fixed so every trace is legible on light tiles.
pub fn vehicle_colour(vehicle_id: &str) -> Color32 {
    let mut hasher = DefaultHasher::new();
    vehicle_id.hash(&mut hasher);
    let hue = (hasher.finish() % 360) as f32 / 360.0;
    Hsva::new(hue, 0.85, 0.75, 1.0).into()
}

impl Plugin for TracesPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui
            .painter()
            .with_clip_rect(response.rect.intersect(ui.clip_rect()));

        for trace in &self.traces {
            let mut pts: Vec<_> = trace
                .points
                .iter()
                .map(|p| projector.project(lon_lat(p.x(), p.y())).to_pos2())
                .collect();

            // Consecutive duplicates tessellate with degenerate normals and
            // render as spike artefacts.
            pts.dedup_by(|a, b| (*a - *b).length_sq() < 0.01);

            let Some(head) = pts.last().copied() else {
                continue;
            };

            if pts.len() >= 2 {
                painter.line(pts, Stroke::new(2.5, trace.colour.gamma_multiply(0.8)));
            }

            painter.circle_filled(head, 4.0, trace.colour);
        }
    }
}
