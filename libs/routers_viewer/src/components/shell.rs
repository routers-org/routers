use routers_codec::osm::OsmNetwork;

use crate::{Component, Input, Map, Matcher, Stack, components::matcher::MatchResult};

pub struct Shell<'a> {
    input: &'a Input<'a>,
    network: &'a OsmNetwork,
}

impl<'a> Shell<'a> {
    pub fn new(network: &'a OsmNetwork, input: &'a Input<'a>) -> Self {
        Self { network, input }
    }
}

impl<'a> Component for Shell<'a> {
    type Output = MatchResult;

    fn draw(&self, ctx: &crate::Context, ui: &mut egui::Ui) -> (egui::Response, Self::Output) {
        let inner = ui.vertical(|ui| {
            ui.set_width(250.);

            let (r1, linestring) = Stack::new(self.input).height(50.).draw(ctx, ui);

            if linestring.is_none() {
                ui.disable();
            }

            let matcher = Matcher::new(self.network, linestring);
            let (r2, matched) = Stack::new(&matcher).height(25.).draw(ctx, ui);

            (r1.union(r2), matched)
        });

        (inner.response, inner.inner.1)
    }
}
