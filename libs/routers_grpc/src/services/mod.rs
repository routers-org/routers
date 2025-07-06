use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};

use routers::Graph;
use routers_codec::{Entry, Metadata};

use std::path::PathBuf;

pub mod matcher;
pub mod optimise;
pub mod proximity;

pub struct RouteService<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub graph: Graph<E, M>,
}

impl RouteService<OsmEntryId, OsmEdgeMetadata> {
    pub fn from_file(file: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let file_os_str = file.as_os_str().to_ascii_lowercase();
        let graph = Graph::new(file_os_str).map_err(|e| format!("{e:?}"))?;

        Ok(RouteService { graph })
    }
}
