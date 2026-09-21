use alloc::sync::Arc;

pub struct RPCAdapter<T> {
    pub(crate) inner: Arc<T>,
}

impl<T> RPCAdapter<T> {
    pub fn new(inner: Arc<T>) -> Self {
        Self { inner }
    }
}

pub mod matcher;
pub mod optimise;
pub mod proximity;
pub mod timezone;

#[cfg(feature = "osm")]
pub struct OsmService;

#[cfg(feature = "osm")]
impl OsmService {
    pub fn from_file(
        file: std::path::PathBuf,
    ) -> Result<routers_codec::osm::OsmNetwork, Box<dyn core::error::Error>> {
        routers_codec::osm::OsmNetwork::from_pbf(&file)
    }
}

#[cfg(feature = "overture")]
pub struct OvertureService;

#[cfg(feature = "overture")]
impl OvertureService {
    pub fn from_file(
        file: std::path::PathBuf,
    ) -> Result<routers_codec::overture::OvertureNetwork, Box<dyn core::error::Error>> {
        routers_codec::overture::OvertureNetwork::from_geoparquet(&file).map_err(Into::into)
    }
}
