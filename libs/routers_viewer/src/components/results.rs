use std::cell::RefCell;

use egui::Response;
use routers::RoutedPath;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};

use crate::utils::{BaseColour, Component, Context};

pub struct Results<'a> {
    path: &'a RoutedPath<OsmEntryId, OsmEdgeMetadata>,
    selected: &'a RefCell<Option<usize>>,
}

impl<'a> Results<'a> {
    pub fn new(
        path: &'a RoutedPath<OsmEntryId, OsmEdgeMetadata>,
        selected: &'a RefCell<Option<usize>>,
    ) -> Self {
        Self { path, selected }
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

                let disc = &self.path.discretized.elements;
                let interp = &self.path.interpolated.elements;

                ui.colored_label(
                    positive,
                    format!(
                        "{} matched point{}, {} road segment{}",
                        disc.len(),
                        if disc.len() == 1 { "" } else { "s" },
                        interp.len(),
                        if interp.len() == 1 { "" } else { "s" },
                    ),
                );

                ui.separator();
                ui.label("Matched Points:");

                egui::ScrollArea::vertical()
                    .id_salt("results_points")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for (i, element) in disc.iter().enumerate() {
                            let is_selected =
                                self.selected.borrow().is_some_and(|v| v == i);

                            let text = format!(
                                "#{i}  ({:.5}, {:.5})",
                                element.point.x, element.point.y
                            );

                            if ui.selectable_label(is_selected, text).clicked() {
                                *self.selected.borrow_mut() = Some(i);
                            }
                        }
                    });

                if let Some(idx) = *self.selected.borrow() {
                    if let Some(element) = disc.get(idx) {
                        ui.separator();
                        ui.heading(format!("Point #{idx}"));
                        ui.colored_label(muted, format!("Lon  {:.6}", element.point.x));
                        ui.colored_label(muted, format!("Lat  {:.6}", element.point.y));
                    }
                }
            })
            .response;

        (response, ())
    }
}
