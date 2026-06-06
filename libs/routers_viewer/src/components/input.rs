use std::cell::RefCell;

use egui::{Frame, Response, TextEdit};
use geo::LineString;
use wkt::Wkt;

use crate::utils::{BaseColour, Component, Size};

pub struct Input<'a> {
    input: &'a RefCell<String>,
}

impl<'a> Input<'a> {
    pub fn new(input: &'a RefCell<String>) -> Self {
        Self { input }
    }
}

impl<'a> Component for Input<'a> {
    type Output = Option<LineString>;

    fn draw(&self, ctx: &crate::utils::Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let input = &mut *self.input.borrow_mut();

        let wkt = input
            .parse::<Wkt>()
            .map_err(|e| anyhow::anyhow!("Failed to parse WKT: {e}"))
            .and_then(|wkt| match wkt {
                Wkt::LineString(ls) => Ok(geo::LineString::from(ls)),
                _ => Err(anyhow::anyhow!("Expected LineString, got {:?}", wkt)),
            });

        let input_box = TextEdit::singleline(input)
            .desired_width(f32::INFINITY)
            .background_color(ctx.scheme().colour(BaseColour::BackgroundRaised));

        let response = Frame::default()
            .inner_margin(ctx.layout().padding(Size::Medium))
            .show(ui, move |ui| {
                ui.label("Input WKT (LineString):");
                ui.add(input_box)
            })
            .response;

        match wkt {
            Ok(ls) => (response, Some(ls)),
            // Suppress the error UI for an empty field — no WKT yet is not an
            // error worth showing.
            Err(_) if input.is_empty() => (response, None),
            Err(error) => {
                ui.disable();
                ui.label(error.to_string());
                (response, None)
            }
        }
    }
}
