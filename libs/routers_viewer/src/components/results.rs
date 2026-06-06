use std::cell::RefCell;

use egui::Response;

use crate::utils::{BaseColour, Component, Context, MatchData};

pub struct Results<'a> {
    data: &'a MatchData,
    selected_layer: &'a RefCell<Option<usize>>,
    selected_candidate: &'a RefCell<Option<usize>>,
}

impl<'a> Results<'a> {
    pub fn new(
        data: &'a MatchData,
        selected_layer: &'a RefCell<Option<usize>>,
        selected_candidate: &'a RefCell<Option<usize>>,
    ) -> Self {
        Self {
            data,
            selected_layer,
            selected_candidate,
        }
    }
}

impl<'a> Component for Results<'a> {
    type Output = ();

    fn draw(&self, ctx: &Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let muted = ctx.scheme().colour(BaseColour::TextMuted);
        let positive = ctx.scheme().colour(BaseColour::Positive);

        let response = ui
            .vertical(|ui| {
                ui.heading("Match Results");
                ui.colored_label(positive, format!("Cost: {}", self.data.cost));
                ui.colored_label(
                    muted,
                    format!(
                        "{} layer{}",
                        self.data.layers.len(),
                        if self.data.layers.len() == 1 { "" } else { "s" },
                    ),
                );

                ui.separator();
                ui.label("Layers:");

                egui::ScrollArea::vertical()
                    .id_salt("results_layers")
                    .max_height(150.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, layer) in self.data.layers.iter().enumerate() {
                            let is_selected = self.selected_layer.borrow().is_some_and(|v| v == i);
                            let n = layer.candidates.len();
                            let text = format!(
                                "Layer {i} — {n} candidate{}",
                                if n == 1 { "" } else { "s" }
                            );
                            if ui.selectable_label(is_selected, text).clicked() {
                                *self.selected_layer.borrow_mut() = Some(i);
                                *self.selected_candidate.borrow_mut() = None;
                            }
                        }
                    });

                if let Some(layer_idx) = *self.selected_layer.borrow() {
                    if let Some(layer) = self.data.layers.get(layer_idx) {
                        ui.separator();
                        ui.heading(format!("Layer {layer_idx} Candidates"));
                        ui.colored_label(
                            muted,
                            format!(
                                "Original: ({:.5}, {:.5})",
                                layer.original.x, layer.original.y
                            ),
                        );

                        egui::ScrollArea::vertical()
                            .id_salt("results_candidates")
                            .max_height(150.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for (i, cand) in layer.candidates.iter().enumerate() {
                                    let is_chosen = layer.chosen_idx == Some(i);
                                    let is_selected =
                                        self.selected_candidate.borrow().is_some_and(|v| v == i);
                                    let label = format!(
                                        "#{i}  emission={}{} ({:.5}, {:.5})",
                                        cand.emission,
                                        if is_chosen { " ✓" } else { "" },
                                        cand.position.x,
                                        cand.position.y,
                                    );
                                    if ui
                                        .selectable_label(is_selected || is_chosen, label)
                                        .clicked()
                                    {
                                        *self.selected_candidate.borrow_mut() = Some(i);
                                    }
                                }
                            });

                        if let Some(cand_idx) = *self.selected_candidate.borrow() {
                            if let Some(cand) = layer.candidates.get(cand_idx) {
                                ui.separator();
                                ui.heading(format!("Candidate #{cand_idx}"));
                                ui.colored_label(muted, format!("Emission={}", cand.emission));
                                ui.colored_label(muted, format!("Lon={:.6}", cand.position.x));
                                ui.colored_label(muted, format!("Lat={:.6}", cand.position.y));
                                if layer.chosen_idx == Some(cand_idx) {
                                    ui.colored_label(positive, "Chosen by solver");
                                }
                            }
                        }
                    }
                }
            })
            .response;

        (response, ())
    }
}
