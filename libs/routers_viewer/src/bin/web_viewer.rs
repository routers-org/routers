//! Browser entry point for the routers viewer.
//!
//! On `wasm32-unknown-unknown` this binary exposes a `#[wasm_bindgen(start)]`
//! function that:
//!
//! 1. Attaches `eframe`'s `WebRunner` to `<canvas id="viewer-canvas">`,
//! 2. Builds a 9-cell [`ShardWindow`](routers_shard::ShardWindow) around an
//!    initial viewport centre and synchronously fetches that centre shard
//!    so the first paint has something to render,
//! 3. Spawns background fetches for the 8 neighbour shards,
//! 4. Wraps the generic [`ViewerApp`](routers_viewer::ViewerApp) in an
//!    outer `App` impl that, every frame, watches `ViewerApp::current_center`
//!    and re-points the window when the user pans into a new cell.
//!
//! Built and bundled by `trunk-ng` — see the workspace's `just web serve`
//! recipe. On native targets `cargo build` still compiles this file, but
//! `main` only prints a hint pointing at the trunk pipeline.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "web_viewer targets wasm32-unknown-unknown — build with \
         `just web serve` (uses `trunk-ng`) or `cargo build \
         --target wasm32-unknown-unknown --bin web_viewer`."
    );
}

// On wasm32 the real work happens in `web::start`, attached via
// `#[wasm_bindgen(start)]`. `main` itself is empty: wasm_bindgen runs
// `start` before `main`, and we have nothing useful to do afterwards.
#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::sync::Arc;

    use eframe::wasm_bindgen::{self, prelude::*};
    use geo::Point;
    use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
    use routers_shard::{
        Geohash, GeohashStrategy, MultiShardNetwork, ShardFetcher, ShardWindow, ShardingStrategy,
        WebShardFetcher,
    };
    use routers_viewer::ViewerApp;
    use walkers::lon_lat;

    const CANVAS_ID: &str = "viewer-canvas";

    /// Where the static shard files live, relative to the document root.
    /// Trunk copies them in from `target/shard_cache/` via the
    /// `<link data-trunk rel="copy-dir">` directive in `index.html`.
    const SHARDS_BASE_URL: &str = "/shards";

    /// The manifest written by `examples/build_shards.rs`. Plain-text,
    /// one filename per line.
    const MANIFEST_PATH: &str = "manifest.txt";

    /// Geohash precision must match the one the build pipeline used.
    const SHARD_PRECISION: u8 = 5;

    /// The viewer holds a composite spanning every currently-loaded
    /// shard, not just the owned cell. This is what unlocks trips that
    /// cross shard boundaries — the matcher sees the union graph.
    type Net = Arc<MultiShardNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>>;
    type Window = ShardWindow<OsmEntryId, OsmEdgeMetadata, GeohashStrategy, WebShardFetcher>;

    fn shard_filename(key: &Geohash) -> String {
        format!("{}.shard.rt", key.0)
    }

    /// Pick a starter filename from the manifest body. Last non-empty
    /// line — preferring later (typically larger, denser) entries is a
    /// good-enough first-paint heuristic.
    fn pick_starter(manifest_body: &str) -> Option<String> {
        manifest_body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .map(str::to_owned)
    }

    /// Parse a `<geohash>.shard.rt` filename back into a `Geohash`.
    fn parse_filename(name: &str) -> Option<Geohash> {
        let stem = name.strip_suffix(".shard.rt")?;
        // Geohash strings are base-32 ASCII (`0123456789bcdefghjkmnpqrstuvwxyz`);
        // accept anything in that alphabet rather than re-validating here.
        if stem.is_empty() {
            return None;
        }
        Some(Geohash(stem.to_owned()))
    }

    fn signal_ready() {
        if let Some(window) = web_sys::window() {
            let _ = window.dispatch_event(
                &web_sys::CustomEvent::new("viewer-ready").expect("CustomEvent"),
            );
        }
    }

    /// Spawn a background fetch for each id in `keys`. Errors are
    /// surfaced via the browser console (the panic hook); a single
    /// failure doesn't kill the others.
    fn spawn_fetches(window: Window, keys: Vec<Geohash>) {
        for key in keys {
            let win = window.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = win.fetch_one(&key).await {
                    log::warn!("background fetch of {key:?} failed: {e:?}");
                }
            });
        }
    }

    use web_time::{Duration, Instant};

    /// Coalesce a burst of shard arrivals into a single composite
    /// rebuild. After a pan, the window typically resolves 8 fetches
    /// inside a few hundred milliseconds; rebuilding eagerly each time
    /// produces a string of long frames. With the debounce we get one
    /// rebuild after the burst settles — one stutter instead of eight,
    /// with the loading overlay explaining why.
    const COMPOSITE_DEBOUNCE: Duration = Duration::from_millis(150);

    /// Outer App wrapper that owns the [`ShardWindow`] alongside the
    /// generic [`ViewerApp`]. Each frame:
    ///
    /// 1. Reads the viewer's current map centre.
    /// 2. `recenter` the window if needed; spawn fetches for new cells.
    /// 3. If the loaded set has drifted from what the composite was
    ///    built over, arm a debounce timer. Once it expires, rebuild
    ///    [`MultiShardNetwork`] and swap it in via `set_network`.
    /// 4. Paint a bottom-right loading indicator showing how many
    ///    shards of the current 9-cell window are in memory.
    /// 5. Delegate to `ViewerApp::update` for the actual UI.
    struct ShardDrivenViewer {
        inner: ViewerApp<Net>,
        window: Window,
        /// Sorted list of geohashes the inner viewer's composite was
        /// built from. Compared against `window.loaded_ids()` to detect
        /// when a rebuild is owed.
        composite_set: Vec<Geohash>,
        /// Target shard count for the current centre — owned + its
        /// strategy-defined neighbours (typically 9, fewer at world
        /// boundaries). Drives the loading overlay's "X/Y" denominator.
        expected: usize,
        /// First time we noticed the loaded set diverging from
        /// `composite_set`. Cleared once the rebuild is committed.
        pending_since: Option<Instant>,
    }

    impl ShardDrivenViewer {
        fn refresh_expected(&mut self) {
            // Owned cell may be `None` very briefly during a recenter;
            // fall back to 1 to avoid showing "0/0" in the overlay.
            self.expected = self
                .window
                .center()
                .map(|c| 1 + self.window.strategy().neighbours(&c).len())
                .unwrap_or(1);
        }
    }

    impl eframe::App for ShardDrivenViewer {
        fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
            // 1. Where is the user looking?
            let pos = self.inner.current_center();
            let point = Point::new(pos.x(), pos.y());

            // 2. Move the window if necessary; spawn fetches for any
            //    cells that just came into scope. Recompute `expected`
            //    when the centre changes (cells at the world edge have
            //    fewer than 8 neighbours).
            let delta = self.window.recenter(point);
            if !delta.unchanged {
                self.refresh_expected();
                if !delta.to_fetch.is_empty() {
                    spawn_fetches(self.window.clone(), delta.to_fetch.clone());
                }
            }

            // 3. Mark a rebuild "owed" whenever the loaded set diverges
            //    from what we last composited. The actual rebuild waits
            //    for the debounce window to elapse, so a cascade of
            //    neighbour fetches turns into a single stutter.
            let mut current_loaded = self.window.loaded_ids();
            current_loaded.sort();
            if current_loaded != self.composite_set && self.pending_since.is_none() {
                self.pending_since = Some(Instant::now());
            }
            if let Some(ts) = self.pending_since {
                if ts.elapsed() >= COMPOSITE_DEBOUNCE && !current_loaded.is_empty() {
                    let shards: Vec<_> = current_loaded
                        .iter()
                        .filter_map(|id| self.window.get(id))
                        .collect();
                    log::info!(
                        "composite rebuild: {} → {} loaded shards",
                        self.composite_set.len(),
                        shards.len()
                    );
                    let composite = MultiShardNetwork::new(shards);
                    // Note: `set_network` clears `match_state`. Acceptable
                    // here because the previous match's candidate ids
                    // may now reference nodes whose graph context just
                    // changed.
                    self.inner.set_network(Arc::new(composite));
                    self.composite_set = current_loaded.clone();
                    self.pending_since = None;
                } else {
                    // Tick again so the debounce timer resolves even if
                    // nothing else triggers a repaint.
                    ctx.request_repaint_after(COMPOSITE_DEBOUNCE);
                }
            }

            // 4. Render the inner viewer first so our overlay sits on
            //    top of the map without being clipped by the central
            //    panel.
            self.inner.update(ctx, frame);

            // 5. Loading indicator. Fixed in the bottom-right via an
            //    egui::Area anchored to the viewport. Stays out of the
            //    way during normal use, narrates the loading state
            //    during a pan.
            let loaded_count = current_loaded.len();
            let is_ready = loaded_count >= self.expected && self.pending_since.is_none();
            let label = if is_ready {
                format!("✅ Ready ({}/{})", loaded_count, self.expected)
            } else {
                format!("⏳ Loading shards {}/{}", loaded_count, self.expected)
            };
            egui::Area::new(egui::Id::new("shard-status"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.label(label);
                        });
                });
        }
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        // `console_log` is overkill for the viewer; egui-wgpu + the panic
        // hook already chatter enough. Reserve a real logger for later
        // if we need filtering.
        let _ = log::set_logger(&CONSOLE_LOGGER);
        log::set_max_level(log::LevelFilter::Info);

        let window = web_sys::window().expect("no global `window`");
        let document = window.document().expect("no `document` on `window`");
        let canvas = document
            .get_element_by_id(CANVAS_ID)
            .unwrap_or_else(|| panic!("missing <canvas id=\"{CANVAS_ID}\">"))
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("element is not an HtmlCanvasElement");

        wasm_bindgen_futures::spawn_local(async move {
            let fetcher = WebShardFetcher::new(SHARDS_BASE_URL);

            // Step 1: discover what's been built.
            let manifest_bytes = fetcher.fetch(MANIFEST_PATH).await.unwrap_or_else(|e| {
                panic!(
                    "failed to fetch {MANIFEST_PATH}: {e:?} — did you run `just web build-shards`?"
                )
            });
            let manifest =
                core::str::from_utf8(&manifest_bytes).expect("manifest is not valid UTF-8");
            let starter_filename = pick_starter(manifest).unwrap_or_else(|| {
                panic!("manifest at {MANIFEST_PATH} is empty — re-run `just web build-shards`")
            });
            let starter_key = parse_filename(&starter_filename).unwrap_or_else(|| {
                panic!("can't parse shard filename `{starter_filename}` — expected d<depth>_<bits>.shard.rt")
            });

            // Step 2: build the ShardWindow at the precision the
            // pipeline used. `recenter` here is just to populate
            // `to_fetch`; the actual initial fetches go through
            // `spawn_fetches` plus a synchronous wait on the centre
            // shard before mounting.
            let strategy = GeohashStrategy::with_precision(SHARD_PRECISION);
            assert_eq!(
                starter_key.0.len(),
                SHARD_PRECISION as usize,
                "starter shard `{}` precision {} doesn't match SHARD_PRECISION {}",
                starter_key.0,
                starter_key.0.len(),
                SHARD_PRECISION
            );
            let starter_centre = {
                let r = strategy.bounds(&starter_key);
                Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
            };
            let shard_window: Window =
                ShardWindow::new(strategy, fetcher, shard_filename as fn(&Geohash) -> String);
            let delta = shard_window.recenter(starter_centre);

            // Step 3: synchronously fetch the centre shard so the viewer
            // mounts with something real. Neighbour fetches run in the
            // background — they trickle in as the user pans into them.
            shard_window
                .fetch_one(&starter_key)
                .await
                .unwrap_or_else(|e| panic!("failed to fetch starter shard {starter_filename}: {e:?}"));
            let starter_shard = shard_window
                .owned()
                .expect("starter shard should be cached after fetch");
            // Log enough about the loaded shard to confirm parity with
            // the native diagnostic (`cargo run -p routers_shard
            // --example debug_match`). If `nodes` / `edges` here
            // differ, the wasm decode is corrupting state.
            log::info!(
                "starter shard {} loaded: {} nodes / {} edges / {} ways with metadata",
                starter_filename,
                starter_shard.num_nodes(),
                starter_shard.graph.edge_count(),
                starter_shard.meta.len(),
            );

            // Build the initial composite over just the starter shard.
            // As neighbour fetches resolve, the loaded-set check in
            // `ShardDrivenViewer::update` will rebuild the composite
            // to include them.
            let initial_net: Net = Arc::new(MultiShardNetwork::new(vec![starter_shard]));
            let initial_loaded: Vec<Geohash> = {
                let mut v = vec![starter_key.clone()];
                v.sort();
                v
            };

            let mut neighbour_keys = delta.to_fetch;
            neighbour_keys.retain(|k| k != &starter_key);
            spawn_fetches(shard_window.clone(), neighbour_keys);

            signal_ready();

            // Step 4: hand control to eframe. The creator closure gets
            // `cc.egui_ctx` once eframe has bootstrapped its rendering
            // context — that's where we build the inner `ViewerApp`.
            // Map starts centred on the starter cell so the first frame
            // shows the loaded data.
            let my_position = lon_lat(starter_centre.x(), starter_centre.y());
            let web_options = eframe::WebOptions::default();
            eframe::WebRunner::new()
                .start(
                    canvas,
                    web_options,
                    Box::new(move |cc| {
                        let inner = ViewerApp::new_at(
                            cc.egui_ctx.clone(),
                            initial_net,
                            my_position,
                        );
                        // Seed `expected` from the starter cell's
                        // neighbour count so the overlay shows the
                        // right denominator before the first recenter
                        // tick runs.
                        let expected =
                            1 + shard_window.strategy().neighbours(&starter_key).len();
                        Ok(Box::new(ShardDrivenViewer {
                            inner,
                            window: shard_window,
                            composite_set: initial_loaded,
                            expected,
                            pending_since: None,
                        }) as Box<dyn eframe::App>)
                    }),
                )
                .await
                .expect("failed to start eframe::WebRunner");
        });
    }

    /// Minimal log → console.* bridge so `log::info!`/`log::warn!`
    /// calls in our shard-driven loop surface in the browser dev tools
    /// without dragging `console_log` (which would do the same thing
    /// with a touch more ceremony).
    struct ConsoleLogger;
    static CONSOLE_LOGGER: ConsoleLogger = ConsoleLogger;

    impl log::Log for ConsoleLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            let line = format!("[{}] {}", record.level(), record.args());
            match record.level() {
                log::Level::Error => web_sys::console::error_1(&line.into()),
                log::Level::Warn => web_sys::console::warn_1(&line.into()),
                log::Level::Info => web_sys::console::info_1(&line.into()),
                log::Level::Debug | log::Level::Trace => web_sys::console::log_1(&line.into()),
            }
        }
        fn flush(&self) {}
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::start;
