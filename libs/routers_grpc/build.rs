use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;

fn find_proto_files<P: AsRef<Path>>(dir: P) -> Vec<PathBuf> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "proto"))
        .map(|entry| entry.path().to_path_buf())
        .map(|path| {
            PathBuf::from_str(&manifest_dir)
                .unwrap()
                .join(path.to_str().unwrap())
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    let routers = find_proto_files("proto");
    let include_dir = PathBuf::from(manifest_dir.clone() + "/proto");
    let includes = [include_dir];

    let mut cfg = prost_build::Config::new();
    cfg.bytes(["."]);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if let Err(e) = tonic_prost_build::configure()
        .file_descriptor_set_path(out_dir.join("routers_descriptor.bin"))
        .use_arc_self(true)
        .message_attribute(
            ".",
            "#[derive(::derive_builder::Builder)] #[builder(setter(into, strip_option), default)]",
        )
        .protoc_arg("--experimental_allow_proto3_optional")
        .include_file("_includes.rs")
        .compile_protos(&routers, &includes)
    {
        eprintln!("Failed to build. {e}");
        tonic_prost_build::configure()
            .file_descriptor_set_path(out_dir.join("routers_descriptor.bin"))
            .message_attribute(".", "#[derive(::derive_builder::Builder)] #[builder(setter(into, strip_option), default)]")
            .use_arc_self(true)
            .include_file("_includes.rs")
            .compile_protos(&routers, &includes)?
    }

    Ok(())
}
