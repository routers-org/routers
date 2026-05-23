//! Browser entry point for the routers viewer.
//!
//! On `wasm32-unknown-unknown` this example exposes a `#[wasm_bindgen(start)]`
//! function that attaches `eframe`'s `WebRunner` to a `<canvas id="viewer-canvas">`
//! in the host page. The network is a stub [`OsmNetwork::empty`] for now;
//! the next step is to fetch shard bytes via `web_sys::fetch` and replace
//! that with a real `ShardedNetwork::from_cached_bytes` load.
//!
//! On native targets this example builds as a no-op stub so
//! `cargo build --examples` keeps working across the workspace.
//!
//! Build with `trunk` (or `wasm-pack`) targeting this example as a cdylib
//! once the surrounding pipeline lands.

// The example is declared as a `cdylib + rlib` (see `Cargo.toml`) so
// `wasm-bindgen` can attach to the `start` function on the web target.
// On native that means there's no implicit `main` — `cargo run --example
// web_viewer` doesn't apply. This stub keeps `cargo build --examples`
// honest by giving rustc something to link against.
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn native_stub() {
    eprintln!(
        "web_viewer targets wasm32-unknown-unknown — build with `trunk serve` \
         or `cargo build --target wasm32-unknown-unknown --example web_viewer`."
    );
}

#[cfg(target_arch = "wasm32")]
mod web {
    use eframe::wasm_bindgen::{self, prelude::*};
    use routers_codec::osm::OsmNetwork;
    use routers_viewer::ViewerApp;

    /// The element ID the host page must expose for `eframe` to bind to.
    const CANVAS_ID: &str = "viewer-canvas";

    #[wasm_bindgen(start)]
    pub fn start() {
        // Route Rust panics to the browser console with a real stack trace
        // instead of an opaque "unreachable executed".
        console_error_panic_hook::set_once();

        let window = web_sys::window().expect("no global `window`");
        let document = window.document().expect("no `document` on `window`");
        let canvas = document
            .get_element_by_id(CANVAS_ID)
            .unwrap_or_else(|| panic!("missing <canvas id=\"{CANVAS_ID}\">"))
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("element is not an HtmlCanvasElement");

        // Placeholder network — empty graph, empty indices. Replaced by a
        // real shard load once the fetch pipeline lands; today this is
        // enough to prove the entry point and the wgpu/WebGPU back end.
        let network = OsmNetwork::empty();

        let web_options = eframe::WebOptions::default();
        wasm_bindgen_futures::spawn_local(async move {
            eframe::WebRunner::new()
                .start(
                    canvas,
                    web_options,
                    Box::new(move |cc| {
                        let ctx = cc.egui_ctx.clone();
                        Ok(Box::new(ViewerApp::new(ctx, network)) as Box<dyn eframe::App>)
                    }),
                )
                .await
                .expect("failed to start eframe::WebRunner");
        });
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::start;
