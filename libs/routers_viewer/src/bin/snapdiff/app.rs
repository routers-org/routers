use std::cell::RefCell;
use std::rc::Rc;

use egui::{Color32, RichText, ScrollArea, SidePanel};
use geo::LineString;
use routers_viewer::{
    ColourFactory, Component, Context, Map, Regular, SharedMapMemory,
    plugins::LineStringPlugin,
};
use walkers::{MapMemory, Plugin, lon_lat};

use crate::diff::{FixtureDiff, Status};

const BASE_LINE: Color32 = Color32::from_rgba_premultiplied(110, 110, 110, 200);
const BASE_SPAN: Color32 = Color32::from_rgba_premultiplied(220, 50, 50, 230);
const HEAD_LINE: Color32 = Color32::from_rgba_premultiplied(0, 100, 255, 200);
const HEAD_SPAN: Color32 = Color32::from_rgba_premultiplied(255, 140, 0, 230);

pub struct SnapDiffApp {
    base_label: String,
    fixtures: Vec<FixtureDiff>,
    selected: Option<usize>,
    changed_only: bool,
    memory: SharedMapMemory,
    base_map: Map,
    head_map: Map,
}

impl SnapDiffApp {
    pub fn new(
        ctx: &eframe::CreationContext<'_>,
        base_label: String,
        mut fixtures: Vec<FixtureDiff>,
    ) -> Self {
        // Severity-first: modified by magnitude, then added/removed, unchanged last.
        fixtures.sort_by(|a, b| {
            let rank = |f: &FixtureDiff| match f.status {
                Status::Modified => 0,
                Status::Added => 1,
                Status::Removed => 2,
                Status::Unchanged => 3,
            };
            rank(a)
                .cmp(&rank(b))
                .then(b.magnitude_m.total_cmp(&a.magnitude_m))
                .then(a.name.cmp(&b.name))
        });

        let tiles = Rc::new(RefCell::new(routers_viewer::tile_source(
            ctx.storage,
            ctx.egui_ctx.clone(),
        )));
        let memory: SharedMapMemory = Rc::new(RefCell::new(MapMemory::default()));

        let home = lon_lat(151.12, -33.52);
        let base_map = Map::with_shared(tiles.clone(), memory.clone(), home);
        let head_map = Map::with_shared(tiles, memory.clone(), home);

        let selected = fixtures.iter().position(|f| f.status.changed());

        let app = Self {
            base_label,
            fixtures,
            selected,
            changed_only: true,
            memory,
            base_map,
            head_map,
        };

        if let Some(idx) = app.selected {
            app.fit_to(idx);
        }

        app
    }

    /// Centre both panes on the union bbox of the fixture's base+head lines.
    fn fit_to(&self, idx: usize) {
        let Some(fixture) = self.fixtures.get(idx) else {
            return;
        };

        let coords = fixture
            .base
            .iter()
            .chain(fixture.head.iter())
            .flat_map(|l| l.0.iter());

        let mut min = (f64::INFINITY, f64::INFINITY);
        let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in coords {
            min = (min.0.min(c.x), min.1.min(c.y));
            max = (max.0.max(c.x), max.1.max(c.y));
        }

        if !min.0.is_finite() {
            return;
        }

        let span = (max.0 - min.0).max(max.1 - min.1).max(1e-4);
        let zoom = ((360.0 / span).log2() - 0.5).clamp(2.0, 19.0);

        let mut memory = self.memory.borrow_mut();
        memory.center_at(lon_lat((min.0 + max.0) / 2.0, (min.1 + max.1) / 2.0));
        let _ = memory.set_zoom(zoom);
    }

    /// The full line plus a thicker overlay per changed span. Spans are padded
    /// by one point each side so a single moved point still draws a segment.
    fn pane_plugins(
        line: Option<&LineString<f64>>,
        spans: &[std::ops::Range<usize>],
        line_colour: Color32,
        span_colour: Color32,
    ) -> Vec<Box<dyn Plugin + 'static>> {
        let Some(line) = line else {
            return Vec::new();
        };

        let mut plugins: Vec<Box<dyn Plugin + 'static>> = Vec::new();

        plugins.push(Box::new(
            LineStringPlugin::new(line.0.clone())
                .color(line_colour)
                .stroke_width(3.0),
        ));

        for span in spans {
            let start = span.start.saturating_sub(1);
            let end = (span.end + 1).min(line.0.len());
            if end - start >= 2 {
                plugins.push(Box::new(
                    LineStringPlugin::new(line.0[start..end].to_vec())
                        .color(span_colour)
                        .stroke_width(5.0),
                ));
            }
        }

        plugins
    }

    /// A line-swatch legend entry: a short stroke in the plugin's colour and
    /// width, followed by what that stroke means on the map.
    fn legend_entry(ui: &mut egui::Ui, colour: Color32, width: f32, text: &str) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 12.0), egui::Sense::hover());
        ui.painter().line_segment(
            [rect.left_center(), rect.right_center()],
            egui::Stroke::new(width, colour),
        );
        ui.label(RichText::new(text).small());
    }

    fn pane_header(
        ui: &mut egui::Ui,
        title: String,
        entries: &[(Color32, f32, &str)],
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong());
            ui.add_space(12.0);
            for (colour, width, text) in entries {
                Self::legend_entry(ui, *colour, *width, text);
                ui.add_space(8.0);
            }
        });
    }

    fn fixture_row_text(fixture: &FixtureDiff) -> String {
        let mut text = format!("{}  {}", fixture.status.badge(), fixture.name);
        if fixture.status == Status::Modified {
            text.push_str(&format!(
                "   Δ{:.0}m  +{}/−{}",
                fixture.magnitude_m, fixture.points_added, fixture.points_removed
            ));
        }
        text
    }
}

impl eframe::App for SnapDiffApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let context = Context {
            scheme: ColourFactory::get_scheme(ctx.theme()),
            layout: Box::new(Regular),
        };

        SidePanel::left("fixtures").show(ctx, |ui| {
            ui.heading("Snapshot Diff");
            ui.label(RichText::new(&self.base_label).monospace().weak());
            ui.separator();

            ui.checkbox(&mut self.changed_only, "Changed only");
            ui.separator();

            let mut clicked = None;
            ScrollArea::vertical().show(ui, |ui| {
                for (idx, fixture) in self.fixtures.iter().enumerate() {
                    if self.changed_only && !fixture.status.changed() {
                        continue;
                    }

                    let selected = self.selected == Some(idx);
                    let mut response =
                        ui.selectable_label(selected, Self::fixture_row_text(fixture));

                    if let Some(error) = &fixture.error {
                        response = response.on_hover_text(
                            RichText::new(error).color(Color32::RED),
                        );
                    }

                    if response.clicked() {
                        clicked = Some(idx);
                    }
                }
            });

            if let Some(idx) = clicked {
                self.selected = Some(idx);
                self.fit_to(idx);
            }

            if let Some(fixture) = self.selected.and_then(|i| self.fixtures.get(i))
                && let Some(error) = &fixture.error
            {
                ui.separator();
                ui.colored_label(Color32::RED, error);
            }
        });

        let fixture = self.selected.and_then(|i| self.fixtures.get(i));

        if let Some(fixture) = fixture {
            self.base_map.set_plugins(Self::pane_plugins(
                fixture.base.as_ref(),
                &fixture.base_spans,
                BASE_LINE,
                BASE_SPAN,
            ));
            self.head_map.set_plugins(Self::pane_plugins(
                fixture.head.as_ref(),
                &fixture.head_spans,
                HEAD_LINE,
                HEAD_SPAN,
            ));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.columns(2, |columns| {
                columns[0].vertical(|ui| {
                    Self::pane_header(
                        ui,
                        format!("BASE — {}", self.base_label),
                        &[
                            (BASE_LINE, 3.0, "matched path (base)"),
                            (BASE_SPAN, 5.0, "removed / changed vs head"),
                        ],
                    );
                    self.base_map.draw(&context, ui);
                });
                columns[1].vertical(|ui| {
                    Self::pane_header(
                        ui,
                        "HEAD — working tree".to_owned(),
                        &[
                            (HEAD_LINE, 3.0, "matched path (head)"),
                            (HEAD_SPAN, 5.0, "added / changed vs base"),
                        ],
                    );
                    self.head_map.draw(&context, ui);
                });
            });
        });
    }
}
