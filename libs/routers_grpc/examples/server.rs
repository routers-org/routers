use routers_fixtures::{LOS_ANGELES, fixture_path, AUSTRALIA, SYDNEY};
use routers_grpc::r#match::MatchServiceServer;
use routers_grpc::optimise::OptimiseServiceServer;
use routers_grpc::scan::ScanServiceServer;
use routers_grpc::{Tracer, proto, services::RouteService};

use tonic_web::GrpcWebLayer;
use tower_http::cors::{Any, CorsLayer};

use dotenv::dotenv;
use std::sync::Arc;
use tonic::codegen::http::Method;
use tonic::transport::Server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load `.env` file
    dotenv()?;

    // Create the tracer first.
    Tracer::init();

    // Create the router
    tracing::info!("Creating Router");
    let los_angeles = fixture_path(AUSTRALIA);
    let router_base = RouteService::from_file(los_angeles).expect("-");
    let router = Arc::new(router_base);

    // Initialize the reflector
    tracing::info!("Router Created");
    let reflector = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build_v1()
        .unwrap();

    // Set the address to serve from
    let addr = "[::1]:9001".parse().unwrap();
    tracing::info!(message = "Starting server.", %addr);

    Server::builder()
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers(Any)
                // allow requests from any origin
                .allow_origin(Any),
        )
        .layer(GrpcWebLayer::new())
        .accept_http1(true)
        .tcp_nodelay(true)
        .add_service(OptimiseServiceServer::from_arc(router.clone()))
        .add_service(MatchServiceServer::from_arc(router.clone()))
        .add_service(ScanServiceServer::from_arc(router.clone()))
        .add_service(reflector)
        .serve(addr)
        .await?;

    Ok(())
}
