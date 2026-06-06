use egui::{Color32, Stroke};
use geo::Coord;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

pub struct DrawPlugin {
    pub points: Vec<Coord>,
    pub cursor: Option<Coord>,
}

impl Plugin for DrawPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        let screen_pts: Vec<_> = self
            .points
            .iter()
            .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
            .collect();

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

        if let Some(cursor) = self.cursor {
            let cursor_pos = projector.project(lon_lat(cursor.x, cursor.y)).to_pos2();

            if let Some(last) = screen_pts.last() {
                painter.line(
                    vec![*last, cursor_pos],
                    Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 165, 0, 120)),
                );
            }

            painter.circle_filled(
                cursor_pos,
                5.0,
                Color32::from_rgba_unmultiplied(255, 165, 0, 160),
            );
            painter.circle_stroke(cursor_pos, 5.0, Stroke::new(1.5, Color32::WHITE));
        }
    }
}
