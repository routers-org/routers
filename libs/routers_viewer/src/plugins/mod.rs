mod candidates;
mod chosen_path;
mod draw;
mod linestring;
mod shard;
mod trace;

pub use candidates::CandidatesPlugin;
pub use chosen_path::ChosenPathPlugin;
pub use draw::DrawPlugin;
pub use linestring::LineStringPlugin;
pub use shard::ShardPlugin;
pub use trace::TracePlugin;

use walkers::{MapMemory, Plugin, Projector};

/// Wraps a type-erased `Box<dyn Plugin>` so it can be handed to
/// `walkers::Map::with_plugin`, which requires `impl Plugin + 'static`.
pub(crate) struct PluginBox(pub Box<dyn Plugin + 'static>);

impl Plugin for PluginBox {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        response: &egui::Response,
        projector: &Projector,
        map_memory: &MapMemory,
    ) {
        let PluginBox(inner) = *self;
        inner.run(ui, response, projector, map_memory);
    }
}
