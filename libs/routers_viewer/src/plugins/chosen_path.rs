use egui::{Color32, Stroke};
use walkers::{MapMemory, Plugin, Projector, lon_lat};

use crate::utils::MatchLayer;

pub struct ChosenPathPlugin {
    pub layers: Vec<MatchLayer>,
}

impl Plugin for ChosenPathPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        for layer in &self.layers {
            let orig = projector
                .project(lon_lat(layer.original.x, layer.original.y))
                .to_pos2();

            if let Some(chosen) = layer.chosen_idx.and_then(|i| layer.candidates.get(i)) {
                let snapped = projector
                    .project(lon_lat(chosen.position.x, chosen.position.y))
                    .to_pos2();

                painter.line(
                    vec![orig, snapped],
                    Stroke::new(1.5, Color32::from_gray(100)),
                );
                painter.circle_filled(snapped, 5.0, Color32::BLUE);
                painter.circle_stroke(snapped, 5.0, Stroke::new(1.0, Color32::BLACK));
            }

            painter.circle_filled(orig, 5.0, Color32::RED);
            painter.circle_stroke(orig, 5.0, Stroke::new(1.0, Color32::BLACK));
        }
    }
}
