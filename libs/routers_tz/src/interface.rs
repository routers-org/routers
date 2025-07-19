use geo::{Point, Polygon};

// todo: docs
#[derive(Copy, Clone)]
pub struct IANATimezoneName(pub &'static str);

// todo: docs
#[derive(Copy, Clone)]
pub struct Timezone {
    pub iana: IANATimezoneName,
}

// todo: docs
pub enum ResolvedTimezones {
    Singular(Timezone),
    Many(Vec<Timezone>),
}

// todo: docs
pub trait TimezoneResolver {
    type Error;

    fn point(&self, point: &Point) -> Result<ResolvedTimezones, Self::Error>;
    fn area(&self, area: &Polygon) -> Result<ResolvedTimezones, Self::Error>;
}
