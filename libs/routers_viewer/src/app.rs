use eframe::CreationContext;
use serde::Serialize;

use crate::{ColourFactory, Component, Context, Regular, Shell};

#[derive(Serialize)]
pub struct Application {
    pub state: Option<u8>,
}

impl Application {
    pub fn new(_: &CreationContext<'_>) -> Self {
        Self { state: None }
    }
}

impl eframe::App for Application {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let scheme = ColourFactory::get_scheme(ctx.theme());

        let context = Context {
            scheme,
            layout: Box::new(Regular),
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            Shell::new().draw(&context, ui);
        });
    }

    #[cfg(feature = "persistence")]
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }
}
