//! Produces a stable fingerprint of the source files that define the
//! `ShardedNetwork` on-disk layout. See `routers_codec/build.rs` for the
//! same idea — the hash becomes part of every shard cache file and lets
//! us reject stale caches with a clear error instead of a `postcard`
//! varint panic.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn fnv1a(bytes: &[u8], h: u64) -> u64 {
    let mut h = h;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let files = [
        "src/network.rs",
        "src/selection.rs",
        "src/strategy/quadtree.rs",
        "src/strategy/geohash.rs",
    ];

    let mut h: u64 = 0xcbf29ce484222325;
    for rel in files {
        let path: &Path = &manifest.join(rel);
        let bytes = fs::read(path)
            .unwrap_or_else(|e| panic!("build.rs: cannot read {} — {e}", path.display()));
        h = fnv1a(rel.as_bytes(), h);
        h = fnv1a(&bytes, h);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("format_hash.rs");
    fs::write(
        &out,
        format!("pub(crate) const FORMAT_HASH: u64 = {h}u64;\n"),
    )
    .unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
