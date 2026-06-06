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
    pub fn new(tiles: HttpTiles, map_memory: MapMemory, position: Position) -> Self {
        Self {
            position,
            tiles: RefCell::new(tiles),
            map_memory: RefCell::new(map_memory),
        }
    }
}

impl Component for Map {
    type Output = ();

    fn draw(&self, _: &Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let tiles = &mut *self.tiles.borrow_mut();
        let memory = &mut *self.map_memory.borrow_mut();

        let map = walkers::Map::new(Some(tiles), memory, self.position);

        let response = ui.add(map);
        (response, ())
    }
}
