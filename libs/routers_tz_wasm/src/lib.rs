//! A WebAssembly **component** for the Routers timezone resolver.
//!
//! The interface is defined in [`wit/world.wit`](../wit/world.wit). It ships
//! apart from the routing component, as the boundary and tz database data is ~2.2 MiB.
//!
//! ```js
//! import { resolver } from "import-path";
//! const [zone] = resolver.getFromPoint({ latitude: -33.87, longitude: 151.21 });
//! zone.ianaCode; // "Australia/Sydney"
//! ```

// wit-bindgen's component export glue contains `unsafe`; the workspace denies
// `unsafe_code` by default, so allow it for this binding crate only.
#![allow(unsafe_code)]

extern crate alloc;

pub mod resolver;

#[cfg(target_arch = "wasm32")]
mod bindings;
