//! The [`Fetcher`] abstraction.
//!
//! A fetcher is just "take a key (string), give me back the bytes". Whether
//! those bytes live on disk, on an HTTP server, or in an in-memory test
//! mock is an implementation detail.

use core::fmt::Debug;
use core::future::Future;

/// Async byte-blob lookup keyed by an opaque string.
///
/// The trait is intentionally narrow — it knows nothing about shard IDs,
/// network types, or transport. [`ShardLoader`](super::ShardLoader) layers
/// the higher-level concerns on top.
pub trait Fetcher {
    type Error: Debug;

    /// Resolve `key` to a byte payload.
    ///
    /// The return type is `impl Future` rather than an `async fn` so the
    /// trait can be used with `!Send` futures on `wasm32` (where the
    /// browser is single-threaded) and `Send` futures on native.
    /// Implementations decide which they produce.
    fn fetch(&self, key: &str) -> impl Future<Output = Result<Vec<u8>, Self::Error>>;
}
