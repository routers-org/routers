use egui::{Color32, Stroke};
use geo::Coord;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

pub struct LineStringPlugin {
    coords: Vec<Coord>,
    pub color: Color32,
    pub stroke_width: f32,
}

impl LineStringPlugin {
    pub fn new(coords: Vec<Coord>) -> Self {
        Self {
            coords,
            color: Color32::BLUE,
            stroke_width: 3.0,
        }
    }

    pub fn color(mut self, color: Color32) -> Self {
        self.color = color;
        self
    }

    pub fn stroke_width(mut self, width: f32) -> Self {
        self.stroke_width = width;
        self
    }
}

impl Plugin for LineStringPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let mut pts: Vec<_> = self
            .coords
            .iter()
            .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
            .collect();

        // Zero-length segments (consecutive duplicate coordinates are common
        // in matched output) give the tessellator degenerate normals, which
        // render as spike artefacts.
        pts.dedup_by(|a, b| (*a - *b).length_sq() < 0.01);

        if pts.len() < 2 {
            return;
        }

        // Clip to the map widget: the ui's clip rect spans the whole panel,
        // so an unclipped stroke bleeds over neighbouring panes.
        ui.painter()
            .with_clip_rect(response.rect.intersect(ui.clip_rect()))
            .line(pts, Stroke::new(self.stroke_width, self.color));
    }
}
