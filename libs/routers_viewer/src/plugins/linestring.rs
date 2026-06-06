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
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        if self.coords.len() < 2 {
            return;
        }

        let pts: Vec<_> = self
            .coords
            .iter()
            .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
            .collect();

        ui.painter()
            .line(pts, Stroke::new(self.stroke_width, self.color));
    }
}
