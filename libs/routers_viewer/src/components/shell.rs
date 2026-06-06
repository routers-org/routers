use routers_codec::osm::OsmNetwork;

use crate::{Component, Map};

pub struct Shell<'a> {
    map: &'a Map,
    network: &'a OsmNetwork,
}

impl<'a> Shell<'a> {
    pub fn new(network: &'a OsmNetwork, map: &'a Map) -> Self {
        Self { network, map }
    }

    // pub fn perform_match(&self) ->
}

impl<'a> Component for Shell<'a> {
    type Output = ();

    fn draw(&self, ctx: &crate::Context, ui: &mut egui::Ui) -> (egui::Response, Self::Output) {
        self.map.draw(ctx, ui)
    }
}
