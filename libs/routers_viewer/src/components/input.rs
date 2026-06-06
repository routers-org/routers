use egui::{Frame, Response, TextEdit};
use wkt::Wkt;

use crate::utils::{BaseColour, Component, Size};

pub struct Input {
    match_wkt: Box<dyn FnOnce(Wkt) -> ()>,
}

impl Input {
    pub fn new(match_wkt: impl FnOnce(Wkt) -> () + 'static) -> Self {
        Self {
            match_wkt: Box::new(match_wkt),
        }
    }
}

impl Component for Input {
    fn draw(self, ctx: &crate::utils::Context, ui: &mut egui::Ui) -> Response {
        let mut wkt_string = String::new();

        Frame::default()
            .inner_margin(ctx.layout().padding(Size::Medium))
            .show(ui, |ui| {
                let input_box = TextEdit::singleline(&mut wkt_string)
                    .desired_width(f32::INFINITY)
                    .background_color(ctx.scheme().colour(BaseColour::BackgroundRaised));

                ui.label("Input WKT (LineString):");
                ui.add(input_box);

                let match_button = ui.button("Match");

                match wkt_string.parse::<Wkt>() {
                    Ok(wkt) => {
                        if match_button.clicked() {
                            (self.match_wkt)(wkt);
                        }
                    }
                    Err(_) => {
                        ui.disable();
                    }
                };
            })
            .response
    }
}
