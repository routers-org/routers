use egui::Response;
use geo::LineString;
use log::info;
use routers::{Match, MatchError, RoutedPath, SolverVariant, r#match::MatchOptions};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork, OsmTripConfiguration};

use crate::utils::Component;

pub struct Matcher<'a> {
    network: &'a OsmNetwork,
    input: Option<LineString>,
}

pub type MatchResult = Result<RoutedPath<OsmEntryId, OsmEdgeMetadata>, MatchError>;

impl<'a> Matcher<'a> {
    pub fn new(network: &'a OsmNetwork, input: Option<LineString>) -> Self {
        Self { network, input }
    }
}

impl<'a> Component for Matcher<'a> {
    type Output = MatchResult;

    fn draw(&self, ctx: &crate::utils::Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let match_button = ui.button("Match!");

        if let Some(linestring) = &self.input
            && match_button.clicked()
        {
            info!("starting match for linestring: {:?}", linestring);
            let runtime = OsmTripConfiguration::default();

            let opts = MatchOptions::new()
                .with_runtime(runtime.clone())
                .with_solver(SolverVariant::Fastest);

            let matched = self.network.r#match(linestring.clone(), opts);

            return (match_button, matched);
        }

        return (match_button, Err(MatchError::NoPointsProvided));
    }
}
