use routers_codec::osm::OsmNetwork;

use crate::{Component, Input, MatchCache, MatchOutput, Matcher, Stack};

pub struct Shell<'a> {
    input: &'a Input<'a>,
    network: &'a OsmNetwork,
    cache: MatchCache,
}

impl<'a> Shell<'a> {
    pub fn new(network: &'a OsmNetwork, input: &'a Input<'a>, cache: MatchCache) -> Self {
        Self { network, input, cache }
    }
}

impl<'a> Component for Shell<'a> {
    type Output = MatchOutput;

    fn draw(&self, ctx: &crate::Context, ui: &mut egui::Ui) -> (egui::Response, Self::Output) {
        let inner = ui.vertical(|ui| {
            ui.set_width(250.);

            let (r1, linestring) = Stack::new(self.input).height(50.).draw(ctx, ui);

            if linestring.is_none() {
                ui.disable();
            }

            let matcher = Matcher::new(self.network, linestring, self.cache.clone());
            let (r2, matched) = Stack::new(&matcher).height(25.).draw(ctx, ui);

            (r1.union(r2), matched)
        });

        (inner.response, inner.inner.1)
    }
}
