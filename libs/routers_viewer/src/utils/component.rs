use crate::utils::colour::ColourScheme;

/// An example of a component that can be drawn using the `Component` trait.
///
/// ```rust
/// // Create your component
/// struct ExampleComponent;
///
/// // Implement the `Component` trait for your component,
/// // providing the `draw` method to render the component.
/// impl Component for ExampleComponent {
///     fn draw<'a>(self, ctx: &'a Context, ui: &mut egui::Ui) -> egui::Response {
///         // Use the context to access the colour scheme, or other context-specific data
///         let colour = ctx.scheme().colour(BaseColour::Text);
///
///         // Draw your component using the `Ui` instance
///         todo!()
///     }
/// }
/// ```
pub trait Component {
    fn draw(self, ctx: &Context, ui: &mut egui::Ui) -> egui::Response;
}

pub struct Context {
    pub scheme: Box<dyn ColourScheme>,
}

impl Context {
    pub fn scheme(&self) -> &dyn ColourScheme {
        &*self.scheme
    }

    pub fn draw(&self, ui: &mut egui::Ui, component: impl Component) -> egui::Response {
        component.draw(self, ui)
    }
}
