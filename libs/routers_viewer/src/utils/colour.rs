use egui::{Color32, Theme};

#[derive(Debug, Clone, Copy)]
pub enum BaseColour {
    Background,
    BackgroundRaised,

    Text,
    TextMuted,

    Positive,
    Negative,
    Pending,

    Muted,
    HalfStrength,

    Strong,
    StrongInverted,
}

pub trait ColourScheme {
    fn colour(&self, color: BaseColour) -> Color32;
}

struct DarkTheme;
struct LightTheme;

impl ColourScheme for DarkTheme {
    fn colour(&self, color: BaseColour) -> Color32 {
        match color {
            BaseColour::Background => Color32::from_rgb(0x2b, 0x2b, 0x2b),
            BaseColour::BackgroundRaised => Color32::from_gray(30),

            BaseColour::Text => Color32::WHITE,
            BaseColour::TextMuted => Color32::LIGHT_GRAY,
            BaseColour::Positive => Color32::GREEN,
            BaseColour::Negative => Color32::RED,
            BaseColour::Pending => Color32::YELLOW,
            BaseColour::Muted => Color32::DARK_GRAY,
            BaseColour::HalfStrength => Color32::BLACK,

            BaseColour::Strong => Color32::BLACK,
            BaseColour::StrongInverted => Color32::WHITE,
        }
    }
}

impl ColourScheme for LightTheme {
    fn colour(&self, color: BaseColour) -> Color32 {
        match color {
            BaseColour::Background => Color32::WHITE,
            BaseColour::BackgroundRaised => Color32::from_gray(240),

            BaseColour::Text => Color32::BLACK,
            BaseColour::TextMuted => Color32::DARK_GRAY,
            BaseColour::Positive => Color32::DARK_GREEN,
            BaseColour::Negative => Color32::RED,
            BaseColour::Pending => Color32::from_rgb(150, 150, 0),
            BaseColour::Muted => Color32::GRAY,
            BaseColour::HalfStrength => Color32::LIGHT_GRAY,

            BaseColour::Strong => Color32::WHITE,
            BaseColour::StrongInverted => Color32::BLACK,
        }
    }
}

pub struct ColourFactory;

impl ColourFactory {
    pub fn get_scheme(theme: Theme) -> Box<dyn ColourScheme>
    where
        Self: Sized,
    {
        match theme.default_visuals().dark_mode {
            true => Box::new(DarkTheme),
            false => Box::new(LightTheme),
        }
    }
}
