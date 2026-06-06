use egui::{Color32, Stroke};
use geo::Coord;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

/// Renders in-progress drawn points (and the connecting line) on the map
/// while draw mode is active. Read-only — click handling is done in app.rs.
pub struct DrawPlugin {
    pub points: Vec<Coord>,
}

impl Plugin for DrawPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        if self.points.is_empty() {
            return;
        }

        let screen_pts: Vec<_> = self
            .points
            .iter()
            .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
            .collect();

        let painter = ui.painter();

        if screen_pts.len() >= 2 {
            painter.line(
                screen_pts.clone(),
                Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 165, 0, 200)),
            );
        }

        for pos in &screen_pts {
            painter.circle_filled(*pos, 5.0, Color32::from_rgb(255, 165, 0));
            painter.circle_stroke(*pos, 5.0, Stroke::new(1.5, Color32::WHITE));
        }
    }
}
