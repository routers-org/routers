use routers_codec::osm::OsmNetwork;
use std::{marker::PhantomData, path::PathBuf};

pub struct GrpcAdapter<T, E, M> {
    inner: T,
    _marker: PhantomData<(E, M)>,
}

impl<T, E, M> GrpcAdapter<T, E, M> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

pub mod matcher;
pub mod optimise;
pub mod proximity;

pub struct OsmService;
impl OsmService {
    pub fn from_file(file: PathBuf) -> Result<OsmNetwork, Box<dyn core::error::Error>> {
        let file_os_str = file.as_os_str().to_ascii_lowercase();
        OsmNetwork::new(file_os_str)
    }
}
