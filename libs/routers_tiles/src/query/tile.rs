use schema::proto::mvt::Tile;

pub struct MVTTile(pub(crate) Tile);

impl From<MVTTile> for Tile {
    fn from(val: MVTTile) -> Self {
        val.0
    }
}
