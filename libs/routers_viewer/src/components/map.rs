use std::cell::RefCell;

use egui::Response;
use walkers::{HttpTiles, MapMemory, Plugin, Position, Projector};

use crate::{Component, Context, plugins::PluginBox};

pub struct Map {
    position: Position,
    tiles: RefCell<HttpTiles>,
    map_memory: RefCell<MapMemory>,
    plugins: RefCell<Vec<Box<dyn Plugin + 'static>>>,
}

impl Map {
    pub fn new(tiles: HttpTiles, map_memory: MapMemory, position: Position) -> Self {
        Self {
            position,
            tiles: RefCell::new(tiles),
            map_memory: RefCell::new(map_memory),
            plugins: RefCell::new(Vec::new()),
        }
    }

    pub fn center_at(&self, position: Position) {
        self.map_memory.borrow_mut().center_at(position);
    }

    /// Replace the plugin list rendered on the next frame. Plugins are consumed
    /// each draw and must be set again every frame if needed.
    pub fn set_plugins(&self, plugins: Vec<Box<dyn Plugin + 'static>>) {
        *self.plugins.borrow_mut() = plugins;
    }

    /// Build a `Projector` for the given clip rect (typically `response.rect`
    /// after drawing). Lets callers convert screen positions to geo coordinates
    /// without needing access to internal map state.
    pub fn projector(&self, clip_rect: egui::Rect) -> Projector {
        Projector::new(clip_rect, &*self.map_memory.borrow(), self.position)
    }
}

impl Component for Map {
    type Output = ();

    fn draw(&self, _: &Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let tiles = &mut *self.tiles.borrow_mut();
        let memory = &mut *self.map_memory.borrow_mut();

        let mut map = walkers::Map::new(Some(tiles), memory, self.position);

        for plugin in self.plugins.borrow_mut().drain(..) {
            map = map.with_plugin(PluginBox(plugin));
        }

        let response = ui.add(map);
        (response, ())
    }
}
