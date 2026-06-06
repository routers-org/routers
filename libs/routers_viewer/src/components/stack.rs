use crate::{Component, Size};

pub struct Stack<'a, C: Component> {
    width: Option<f32>,
    height: Option<f32>,

    component: &'a C,
}

impl<'a, C: Component> Stack<'a, C> {
    pub fn new(component: &'a C) -> Self {
        Self {
            width: None,
            height: None,
            component,
        }
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }
}

impl<'a, C: Component> Component for Stack<'a, C> {
    type Output = C::Output;

    fn draw(&self, ctx: &crate::Context, ui: &mut egui::Ui) -> (egui::Response, C::Output) {
        let frame = egui::containers::Frame::default()
            .outer_margin(ctx.layout().padding(Size::Small))
            .inner_margin(ctx.layout().padding(Size::Small))
            .show(ui, |ui| {
                if let Some(width) = self.width {
                    ui.set_width(width);
                }

                if let Some(height) = self.height {
                    ui.set_height(height);
                }

                self.component.draw(ctx, ui)
            });

        (frame.response, frame.inner.1)
    }
}
