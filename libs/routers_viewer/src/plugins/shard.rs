use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use std::collections::HashSet;
use walkers::{MapMemory, Plugin, Projector, lon_lat};

pub struct ShardPlugin {
    shards: Vec<Geohash>,
    active: HashSet<Geohash>,
}

impl ShardPlugin {
    pub fn new(shards: Vec<Geohash>, active: HashSet<Geohash>) -> Self {
        Self { shards, active }
    }
}

impl Plugin for ShardPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();
        let viewport = ui.clip_rect();
        let strategy = GeohashStrategy::with_precision(4);

        // Inactive shard colours.
        let fill_inactive   = Color32::from_rgba_unmultiplied(100, 120, 160, 18);
        let border_inactive = Color32::from_rgba_unmultiplied(100, 120, 160, 170);
        let label_color     = Color32::from_rgba_unmultiplied( 50,  60,  90, 210);

        // Active shard (has received traffic) — slightly greener tint.
        let fill_active     = Color32::from_rgba_unmultiplied( 60, 160,  80, 30);
        let border_active   = Color32::from_rgba_unmultiplied( 60, 160,  80, 210);

        for shard in &self.shards {
            let bounds = strategy.bounds(shard);
            let min = bounds.min();
            let max = bounds.max();

            let tl = proj(projector, min.x, max.y);
            let tr = proj(projector, max.x, max.y);
            let br = proj(projector, max.x, min.y);
            let bl = proj(projector, min.x, min.y);

            let screen_rect = Rect::from_points(&[tl, tr, br, bl]);
            if !viewport.intersects(screen_rect) {
                continue;
            }

            let is_active = self.active.contains(shard);
            let (fill, border_color) = if is_active {
                (fill_active, border_active)
            } else {
                (fill_inactive, border_inactive)
            };

            painter.add(egui::Shape::convex_polygon(
                vec![tl, tr, br, bl],
                fill,
                Stroke::NONE,
            ));

            let stroke = Stroke::new(1.5, border_color);
            painter.line_segment([tl, tr], stroke);
            painter.line_segment([tr, br], stroke);
            painter.line_segment([br, bl], stroke);
            painter.line_segment([bl, tl], stroke);

            if screen_rect.width() > 36.0 {
                let center = Pos2::new(
                    (tl.x + tr.x + br.x + bl.x) * 0.25,
                    (tl.y + tr.y + br.y + bl.y) * 0.25,
                );
                let font = FontId::proportional(12.0);
                let label = shard.to_string();
                // egui has no built-in bold variant; double-paint with a 0.5px
                // horizontal offset gives a convincing pseudo-bold appearance.
                for dx in [0.0_f32, 0.5] {
                    painter.text(
                        center + egui::vec2(dx, 0.0),
                        Align2::CENTER_CENTER,
                        &label,
                        font.clone(),
                        label_color,
                    );
                }
            }
        }
    }
}

fn proj(projector: &Projector, lon: f64, lat: f64) -> Pos2 {
    projector.project(lon_lat(lon, lat)).to_pos2()
}
