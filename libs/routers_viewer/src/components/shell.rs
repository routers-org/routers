use std::sync::Arc;

use routers_codec::osm::OsmNetwork;
use walkers::MapMemory;

use crate::{Component, Map};

pub struct Shell {
    network: Arc<OsmNetwork>,
}

impl Shell {
    pub fn new(network: Arc<OsmNetwork>) -> Self {
        Self { network }
    }

    // pub fn perform_match(&self) ->
}

impl Component for Shell {
    fn draw(self, ctx: &crate::Context, ui: &mut egui::Ui) -> egui::Response {
        ui.label(format!("ROUTERS"))
    }
}
