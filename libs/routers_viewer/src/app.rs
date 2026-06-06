use std::{cell::RefCell, path::PathBuf};

use anyhow::Context as _;
use eframe::CreationContext;
use egui::SidePanel;
use log::{info, log};
use routers::{Match, MatchError};
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{SYDNEY, fixture};
use walkers::{
    HttpTiles, MapMemory,
    sources::{Mapbox, MapboxStyle, OpenStreetMap},
};

const FIXTURE_NETWORK: &'static str = "fixture-network";
const MAPBOX_API_KEY: &'static str = "mapbox-api-key";

use crate::{ColourFactory, Component, Context, Input, Map, Regular, Shell};

pub struct Application {
    network: OsmNetwork,
    map: Map,

    input_string: RefCell<String>,
}

impl Application {
    pub fn new(ctx: &CreationContext<'_>) -> anyhow::Result<Self> {
        let storage = ctx
            .storage
            .context("was not compiled with storage feature")?;

        let api_key = storage
            .get_string(MAPBOX_API_KEY)
            .context("could not find mapbox API key")
            .ok();

        let default_path = fixture!(SYDNEY).clone();
        let path = storage
            .get_string(FIXTURE_NETWORK)
            .map(|v| PathBuf::from(v))
            .unwrap_or(default_path);

        path.try_exists()
            .context(path.to_string_lossy().to_string())
            .context("The path must point to a valid file.")?;

        info!("Opening road network at {:?}...", path);
        let network = OsmNetwork::from_pbf(&path).map_err(|e| anyhow::anyhow!("{}", e))?;

        let egui_ctx = ctx.egui_ctx.clone();
        let tiles = match api_key {
            Some(key) => HttpTiles::new(
                Mapbox {
                    style: MapboxStyle::Light,
                    high_resolution: true,
                    access_token: key,
                },
                egui_ctx,
            ),
            None => HttpTiles::new(OpenStreetMap, egui_ctx),
        };

        let memory = MapMemory::default();
        let center = walkers::lon_lat(151.12, -33.52);

        let map = Map::new(tiles, memory, center);

        Ok(Self {
            map,
            network,
            input_string: RefCell::new(String::new()),
        })
    }
}

impl eframe::App for Application {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let scheme = ColourFactory::get_scheme(ctx.theme());

        let context = Context {
            scheme,
            layout: Box::new(Regular),
        };

        let input = Input::new(&self.input_string);

        SidePanel::left("controls").show(ctx, |ui| {
            let (_, result) = Shell::new(&self.network, &input).draw(&context, ui);

            match result {
                Err(MatchError::NoPointsProvided) => {}
                Ok(_) => {
                    info!("matched path: {:#?}", result);
                }
                Err(err) => {
                    log::error!("match error: {}", err);
                }
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| self.map.draw(&context, ui));
    }
}
