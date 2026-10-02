//! A WebAssembly **component** for the Routers engine.
//!
//! The interface is defined in [`wit/world.wit`](../wit/world.wit) and consumed as a component.
//! So, one can simply call in from a WASM runtime, like the Web.
//!
//! ```js
//! import { router } from "import-path";
//! const engine = new router.Engine();
//!
//! engine.loadShard(id, /* Shard Data */);
//!
//! // Perform map-matching
//! const _ = engine.match(trace, { searchDistance: 50, reachDistance: 5000 });
//! // Perform point-to-point routing
//! const _ = engine.route(start, end);
//! ```
//!

// wit-bindgen's component export glue contains `unsafe`; the workspace denies
// `unsafe_code` by default, so allow it for this binding crate only.
#![allow(unsafe_code)]

extern crate alloc;

pub mod engine;

#[cfg(target_arch = "wasm32")]
mod bindings;
