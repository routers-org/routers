/// The standard service definitions
#[allow(clippy::all)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/_includes.rs"));

    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("routers_descriptor");
}

pub mod api {
    pub use super::proto::api::*;
}

pub mod model {
    pub use super::proto::model::v1::*;
}

pub mod optimise {
    pub use super::api::optimise::v1::optimise_service_client::OptimiseServiceClient;
    pub use super::api::optimise::v1::optimise_service_server::OptimiseService;
    pub use super::api::optimise::v1::optimise_service_server::OptimiseServiceServer;

    pub use super::api::optimise::v1::*;
}

pub mod r#match {
    pub use super::api::r#match::v1::match_service_client::MatchServiceClient;
    pub use super::api::r#match::v1::match_service_server::MatchService;
    pub use super::api::r#match::v1::match_service_server::MatchServiceServer;

    pub use super::api::r#match::v1::*;
}

pub mod scan {
    pub use super::api::scan::v1::scan_service_client::ScanServiceClient;
    pub use super::api::scan::v1::scan_service_server::ScanService;
    pub use super::api::scan::v1::scan_service_server::ScanServiceServer;

    pub use super::api::scan::v1::*;
}
