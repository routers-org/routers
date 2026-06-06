use std::cell::RefCell;

use egui::{Frame, Response, TextEdit};
use wkt::Wkt;

use crate::utils::{BaseColour, Component, Size};

pub struct Input {
    input: RefCell<String>,
}

impl Input {
    pub fn new(input: RefCell<String>) -> Self {
        Self { input }
    }
}

impl Component for Input {
    type Output = Option<Wkt>;

    fn draw(&self, ctx: &crate::utils::Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let input = &mut *self.input.borrow_mut();

        let input_box = TextEdit::singleline(input)
            .desired_width(f32::INFINITY)
            .background_color(ctx.scheme().colour(BaseColour::BackgroundRaised));

        let wkt = self.input.borrow().parse::<Wkt>();
        let output = wkt.clone().ok();

        let response = Frame::default()
            .inner_margin(ctx.layout().padding(Size::Medium))
            .show(ui, move |ui| {
                ui.label("Input WKT (LineString):");
                ui.add(input_box);

                if let Err(error) = wkt {
                    ui.disable();
                    ui.label(error);
                }

                ui.button("Match")
            })
            .response;

        (response, output)
    }
}
