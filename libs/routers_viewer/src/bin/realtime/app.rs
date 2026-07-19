use eframe::{App, CreationContext};

pub struct RealtimeApp {}

impl RealtimeApp {
    pub fn new<'a>(_ctx: &'a CreationContext<'a>) -> Self {
        Self {}
    }
}

impl App for RealtimeApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {}
}
