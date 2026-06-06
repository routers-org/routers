use std::cell::RefCell;

use egui::Response;
use walkers::{HttpTiles, MapMemory, Position};

use crate::{Component, Context};

pub struct Map {
    position: Position,
    tiles: RefCell<HttpTiles>,
    map_memory: RefCell<MapMemory>,
}

impl Map {
    pub fn new(
        position: Position,
        tiles: RefCell<HttpTiles>,
        map_memory: RefCell<MapMemory>,
    ) -> Self {
        Self {
            position,
            tiles,
            map_memory,
        }
    }
}

impl Component for Map {
    fn draw(self, ctx: &Context, ui: &mut egui::Ui) -> Response {
        let tiles = &mut *self.tiles.borrow_mut();
        let memory = &mut *self.map_memory.borrow_mut();

        let map = walkers::Map::new(Some(tiles), memory, self.position);

        ui.add(map)
    }
}
