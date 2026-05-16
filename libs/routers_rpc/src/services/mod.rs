use alloc::sync::Arc;
use routers_codec::osm::OsmNetwork;
use std::{marker::PhantomData, path::PathBuf};

pub struct RPCAdapter<T, E, M> {
    pub(crate) inner: Arc<T>,
    _marker: PhantomData<(E, M)>,
}

impl<T, E, M> RPCAdapter<T, E, M> {
    pub fn new(inner: Arc<T>) -> Self {
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
        OsmNetwork::from_pbf(&file)
    }
}
