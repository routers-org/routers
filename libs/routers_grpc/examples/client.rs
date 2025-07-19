use routers_fixtures::{LAX_LYNWOOD_TRIP, WHALE_MASCOT_TRIP};
use routers_grpc::r#match::*;

use std::fs::File;
use std::io::Write;

use geo::LineString;
use wkt::{ToWkt, TryFromWkt};

use routers_grpc::sdk;
use tonic::Request;
use tonic::transport::Channel;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channel = Channel::from_static("http://[::1]:9001").connect().await?;
    let mut client = MatchServiceClient::new(channel);

    let linestring = LineString::try_from_wkt_str(WHALE_MASCOT_TRIP)?;

    let route = Request::new(MatchRequest {
        data: Into::<sdk::Coordinates>::into(linestring).clone(),
        ..Default::default()
    });

    let response = client.r#match(route).await?;

    let matched = response
        .get_ref()
        .discretized()
        .ok_or("no linestring value")?;
    let interpolated = response
        .get_ref()
        .interpolated()
        .ok_or("no interpolated value")?;
    println!("Routed points: {}", matched.linestring().wkt_string());

    let path = "routed.wkt";
    let mut output = File::create(path).unwrap();
    write!(output, "{}", interpolated.linestring().wkt_string()).expect("must write");

    Ok(())
}
