use prost_build::Config;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = ["proto/osm/fileformat.proto", "proto/osm/osmformat.proto"];

    let includes = ["proto"];
    let mut cfg = Config::new();
    cfg.bytes(["."]);

    if let Err(e) = cfg
        .protoc_arg("--experimental_allow_proto3_optional")
        .enum_attribute(".", "#[derive(::serde::Serialize)]")
        .compile_protos(&protos, &includes)
    {
        eprintln!("Failed to build. {e}");
        cfg.compile_protos(&protos, &includes)?
    }

    Ok(())
}
