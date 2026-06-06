use std::{path::PathBuf, sync::Arc};

use anyhow::Context as _;
use eframe::CreationContext;
use log::info;
use quick_error::ResultExt;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{SYDNEY, fixture};
use serde::Serialize;
use walkers::{
    HttpTiles, MapMemory,
    sources::{Mapbox, MapboxStyle},
};

const FIXTURE_NETWORK: &'static str = "fixture-network";
const MAPBOX_API_KEY: &'static str = "mapbox-api-key";

use crate::{ColourFactory, Component, Context, Regular, Shell};

#[derive(Serialize)]
pub struct Application {
    #[serde(skip)]
    network: Arc<OsmNetwork>,
    #[serde(skip)]
    map_tiles: Option<HttpTiles>,
    map_memory: MapMemory,
}

impl Application {
    pub fn new(ctx: &CreationContext<'_>) -> anyhow::Result<Self> {
        let storage = ctx
            .storage
            .context("was not compiled with storage feature")?;

        let api_key = storage
            .get_string(MAPBOX_API_KEY)
            .context("could not find mapbox API key")?;

        let default_path = fixture!(SYDNEY).clone();
        let path = storage
            .get_string(FIXTURE_NETWORK)
            .map(|v| PathBuf::from(v))
            .unwrap_or(default_path);

        if !path.exists() {
            return anyhow::bail!("The path (value={:?}) must point to a valid file.", path);
        }

        info!("Opening road network at {:?}...", path);
        let network = OsmNetwork::from_saved(&path).map_err(|e| anyhow::anyhow!("{}", e))?;

        let tiles = HttpTiles::new(
            Mapbox {
                style: MapboxStyle::Light,
                high_resolution: true,
                access_token: api_key,
            },
            ctx.egui_ctx.clone(),
        );

        Ok(Self {
            network: Arc::new(network),
            map_memory: MapMemory::default(),
            map_tiles: Some(tiles),
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

        egui::CentralPanel::default().show(ctx, |ui| {
            Shell::new(self.network.clone()).draw(&context, ui);
        });
    }

    #[cfg(feature = "persistence")]
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }
}
