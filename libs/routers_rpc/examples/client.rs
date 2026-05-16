use connectrpc::client::{ClientConfig, HttpClient};
use routers_fixtures::LAX_LYNWOOD_TRIP;
use routers_rpc::sdk::r#match::coordinate;
use schema::{
    connect::routers::api::r#match::v1::MatchServiceClient,
    proto::routers::api::r#match::v1::MatchRequest,
};

use std::fs::File;
use std::io::Write;

use geo::{Coord, LineString};
use wkt::{ToWkt, TryFromWkt};

fn route_to_linestring(elements: &[schema::proto::routers::model::v1::RouteElement]) -> LineString {
    elements
        .iter()
        .filter_map(|element| element.coordinate.as_option())
        .map(|c| Coord {
            x: c.longitude,
            y: c.latitude,
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn core::error::Error>> {
    let http = HttpClient::plaintext();
    let config = ClientConfig::new("http://[::1]:9001".parse()?);

    let client = MatchServiceClient::new(http, config);

    let linestring = LineString::try_from_wkt_str(LAX_LYNWOOD_TRIP)?;
    let request = MatchRequest {
        data: linestring.into_iter().map(coordinate).collect(),
        ..Default::default()
    };

    let response = client.r#match(request).await?.into_owned();
    let first = response.matches.first().ok_or("no matches returned")?;

    let discretized = route_to_linestring(&first.discretized);
    let interpolated = route_to_linestring(&first.interpolated);

    println!("Routed points: {}", discretized.wkt_string());

    let mut output = File::create("routed.wkt")?;
    write!(output, "{}", interpolated.wkt_string())?;

    Ok(())
}
