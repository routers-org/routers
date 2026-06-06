use crate::Component;

pub struct Shell {}

impl Shell {
    pub fn new() -> Self {
        Self {}
    }
}

impl Component for Shell {
    fn draw(self, ctx: &crate::Context, ui: &mut egui::Ui) -> egui::Response {
        ui.label(format!("ROUTERS"))
    }
}
