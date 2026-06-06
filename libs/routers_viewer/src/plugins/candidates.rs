use egui::{Align2, Color32, FontId, Stroke};
use walkers::{MapMemory, Plugin, Projector, lon_lat};

use crate::utils::MatchLayer;

pub struct CandidatesPlugin {
    pub layer: MatchLayer,
    pub selected_idx: Option<usize>,
}

impl Plugin for CandidatesPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();
        let font = FontId::proportional(11.0);

        let orig = projector
            .project(lon_lat(self.layer.original.x, self.layer.original.y))
            .to_pos2();

        painter.circle_filled(orig, 8.0, Color32::RED);
        painter.circle_stroke(orig, 8.0, Stroke::new(2.0, Color32::BLACK));
        painter.text(
            orig - egui::vec2(0.0, 14.0),
            Align2::CENTER_BOTTOM,
            "Original",
            font.clone(),
            Color32::RED,
        );

        for (i, cand) in self.layer.candidates.iter().enumerate() {
            let pos = projector
                .project(lon_lat(cand.position.x, cand.position.y))
                .to_pos2();

            let is_chosen = self.layer.chosen_idx == Some(i);
            let is_selected = self.selected_idx == Some(i);

            let color = if is_selected {
                Color32::from_rgb(255, 165, 0)
            } else if is_chosen {
                Color32::BLUE
            } else {
                Color32::GRAY
            };

            let radius = if is_chosen || is_selected { 7.0 } else { 4.0 };

            painter.circle_filled(pos, radius, color);
            painter.circle_stroke(pos, radius, Stroke::new(1.0, Color32::BLACK));
            painter.text(
                pos + egui::vec2(radius + 2.0, 0.0),
                Align2::LEFT_CENTER,
                format!("{}", cand.emission),
                font.clone(),
                color,
            );

            if is_chosen {
                painter.text(
                    pos - egui::vec2(0.0, radius + 4.0),
                    Align2::CENTER_BOTTOM,
                    "Match",
                    font.clone(),
                    Color32::BLUE,
                );
            }
        }
    }
}
