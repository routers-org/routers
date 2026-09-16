use core::time::Duration;
use std::sync::mpsc::Receiver;

use eframe::CreationContext;
use egui::SidePanel;
use futures::StreamExt;
use routers_realtime::bus::NATSStream;
use routers_realtime::protocol::{CommittedOutput, OutputKind};
use walkers::{MapMemory, lon_lat};

use routers_viewer::{ColourFactory, Component, Context, Map, Regular};

use crate::E;
use crate::plugin::{TraceLine, TracesPlugin, vehicle_colour};
use crate::store::{StoreStats, TraceStore};

/// Maximum events drained from the channel per frame, so a burst can't
/// stall the UI thread.
const DRAIN_PER_FRAME: usize = 2_000;

pub struct RealtimeApp {
    map: Map,
    store: TraceStore,
    rx: Receiver<CommittedOutput<E>>,
    centered: bool,
}

impl RealtimeApp {
    pub fn new(
        ctx: &CreationContext<'_>,
        mut source: NATSStream<CommittedOutput<E>>,
        trace_capacity: usize,
        idle_ttl: Duration,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let egui_ctx = ctx.egui_ctx.clone();

        // Pump the injected stream onto the UI thread, waking egui so the
        // frame draws as soon as data arrives.
        tokio::spawn(async move {
            while let Some(result) = source.next().await {
                if tx.send(result).is_err() {
                    // UI thread has gone away; stop pumping.
                    break;
                }
                egui_ctx.request_repaint();
            }
        });

        let tiles = routers_viewer::tile_source(ctx.storage, ctx.egui_ctx.clone());

        Self {
            map: Map::new(tiles, MapMemory::default(), lon_lat(151.2, -33.87)),
            store: TraceStore::new(trace_capacity, idle_ttl),
            rx,
            centered: false,
        }
    }
}

impl eframe::App for RealtimeApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let context = Context {
            scheme: ColourFactory::get_scheme(ctx.theme()),
            layout: Box::new(Regular),
        };

        for output in self.rx.try_iter().take(DRAIN_PER_FRAME) {
            // Recentre once, on the first matched layer we see.
            if !self.centered
                && let OutputKind::Matched { diff, .. } = &output.kind
                && let Some(layer) = diff.layers.first()
            {
                self.map
                    .center_at(lon_lat(layer.position.x(), layer.position.y()));
                self.centered = true;
            }

            self.store.ingest(output);
        }
        self.store.evict_idle();

        SidePanel::left("realtime_stats")
            .resizable(false)
            .exact_width(180.0)
            .show(ctx, |ui| draw_stats(ui, &self.store.stats()));

        let traces = self
            .store
            .traces
            .iter()
            .map(|(vehicle_id, trace)| TraceLine {
                colour: vehicle_colour(*vehicle_id),
                points: trace.flattened(),
            })
            .collect();

        self.map
            .set_plugins(vec![Box::new(TracesPlugin::new(traces))]);

        egui::CentralPanel::default().show(ctx, |ui| {
            self.map.draw(&context, ui);
        });

        // Keep eviction and the events/s gauge ticking even when no events
        // arrive to wake us.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

fn draw_stats(ui: &mut egui::Ui, stats: &StoreStats) {
    ui.add_space(4.0);
    ui.heading("Realtime");
    ui.separator();

    ui.label(format!("Vehicles:  {}", stats.vehicle_count));
    ui.label(format!("Events/s:  {}", stats.events_per_sec));
    ui.label(format!("Total:     {}", stats.total_events));
}
