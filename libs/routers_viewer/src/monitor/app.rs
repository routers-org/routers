use std::sync::{Arc, Mutex};

use eframe::CreationContext;
use egui::{Color32, SidePanel};
use routers_shard::Geohash;
use walkers::{HttpTiles, MapMemory, lon_lat, sources::OpenStreetMap};

use crate::{
    ColourFactory, Component, Context, Map, Regular,
    monitor::{
        nats::{self, InboundMessage},
        store::{StoreStats, VehicleTraceStore},
    },
    plugins::{ShardPlugin, TracePlugin},
};

pub struct MonitorApp {
    map: Map,
    store: Arc<Mutex<VehicleTraceStore>>,
    rx: std::sync::mpsc::Receiver<InboundMessage>,
    shards: Vec<Geohash>,
}

impl MonitorApp {
    pub fn new(ctx: &CreationContext<'_>) -> anyhow::Result<Self> {
        let nats_url =
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());

        let (tx, rx) = std::sync::mpsc::channel();
        let egui_ctx = ctx.egui_ctx.clone();

        tokio::spawn(async move {
            if let Err(e) = nats::subscribe(nats_url, tx, egui_ctx).await {
                log::error!("NATS subscription error: {e}");
            }
        });

        let tiles = HttpTiles::new(OpenStreetMap, ctx.egui_ctx.clone());
        let shards = load_shards();

        Ok(Self {
            map: Map::new(tiles, MapMemory::default(), lon_lat(151.12, -33.52)),
            store: Arc::new(Mutex::new(VehicleTraceStore::new())),
            rx,
            shards,
        })
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let context = Context {
            scheme: ColourFactory::get_scheme(ctx.theme()),
            layout: Box::new(Regular),
        };

        {
            let mut store = self.store.lock().unwrap();
            for msg in self.rx.try_iter().take(2_000) {
                match msg {
                    InboundMessage::Context(mc) => store.ingest_context(mc),
                    InboundMessage::Result(mr) => store.ingest_result(mr),
                }
            }
            store.evict_stale();
        }

        let (stats, active_shards) = {
            let store = self.store.lock().unwrap();
            (store.stats(), store.active_shards.clone())
        };

        SidePanel::left("monitor_stats")
            .resizable(false)
            .exact_width(180.0)
            .show(ctx, |ui| {
                draw_stats(ui, &stats, self.shards.len());
            });

        self.map.set_plugins(vec![
            Box::new(ShardPlugin::new(self.shards.clone(), active_shards)),
            Box::new(TracePlugin::new(Arc::clone(&self.store))),
        ]);

        egui::CentralPanel::default().show(ctx, |ui| {
            self.map.draw(&context, ui);
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

fn load_shards() -> Vec<Geohash> {
    // Candidates in priority order: env var, then common relative locations.
    let candidates: Vec<std::path::PathBuf> = {
        let mut v = Vec::new();
        if let Ok(dir) = std::env::var("SHARD_DIR") {
            v.push(std::path::PathBuf::from(dir));
        }
        // Workspace-relative paths when running from various cwd locations.
        for rel in &["target/shard_cache", "../../target/shard_cache", "../../../target/shard_cache"] {
            v.push(std::path::PathBuf::from(rel));
        }
        v
    };

    for dir in &candidates {
        let manifest = dir.join("manifest.txt");
        if let Ok(content) = std::fs::read_to_string(&manifest) {
            let shards: Vec<Geohash> = content
                .lines()
                .filter_map(|line| {
                    let name = line.trim().strip_suffix(".shard.rt").unwrap_or(line.trim());
                    name.parse::<Geohash>().ok()
                })
                .collect();
            eprintln!("[monitor] loaded {} shards from {}", shards.len(), manifest.display());
            return shards;
        }
    }

    eprintln!(
        "[monitor] no shard manifest found — set SHARD_DIR to your shard cache directory. \
         Tried: {}",
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    vec![]
}

fn draw_stats(ui: &mut egui::Ui, stats: &StoreStats, shard_count: usize) {
    ui.add_space(4.0);
    ui.heading("Monitor");
    ui.separator();

    ui.label(format!("Vehicles:  {}", stats.vehicle_count));
    ui.label(format!("Events/s:  {:.0}", stats.events_per_sec));
    ui.label(format!("Shards:    {}", shard_count));

    ui.add_space(6.0);
    ui.separator();

    let total = stats.success + stats.no_candidate + stats.error;
    let pct = |n: u64| {
        if total > 0 {
            n as f32 / total as f32 * 100.0
        } else {
            0.0
        }
    };

    ui.colored_label(
        Color32::from_rgb(80, 200, 80),
        format!("✓  {:.1}%  matched", pct(stats.success)),
    );
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!("○  {:.1}%  no candidate", pct(stats.no_candidate)),
    );
    ui.colored_label(
        Color32::from_rgb(220, 60, 60),
        format!("✗  {:.1}%  error", pct(stats.error)),
    );
}
