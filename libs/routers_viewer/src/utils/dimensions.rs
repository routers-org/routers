use egui::Margin;

#[derive(Debug, Clone, Copy)]
pub enum Size {
    Small,
    Medium,
    Large,
}

pub trait Layout {
    fn padding(&self, size: Size) -> Margin;
}

pub struct Regular;

impl Layout for Regular {
    fn padding(&self, size: Size) -> Margin {
        Margin::same(match size {
            Size::Small => 2,
            Size::Medium => 5,
            Size::Large => 9,
        })
    }
}
