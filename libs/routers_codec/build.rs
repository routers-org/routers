//! Builds a stable fingerprint of the source files that determine the
//! `OsmNetwork` on-disk layout. The fingerprint becomes part of every
//! cache file written by [`OsmNetwork::save_to_file`], so any change to
//! these files automatically invalidates older caches with a clear error
//! instead of a `postcard` panic.
//!
//! Files outside this list (parsers, error types, the iterator machinery)
//! can change freely without touching cache compatibility — the goal is
//! "only regenerate when needed", not "regenerate on every recompile".

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// FNV-1a 64-bit. Deterministic across builds and platforms (unlike
/// `DefaultHasher`, which is randomised). No dependency needed.
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
    // Curated list — only files that define types appearing in the
    // serialised payload of `OsmNetwork`.
    let files = [
        "src/osm/graph.rs",
        "src/osm/element/variants/mod.rs",
        "src/osm/element/variants/way.rs",
        "src/osm/element/variants/node.rs",
        "src/osm/element/variants/relation.rs",
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
