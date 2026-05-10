pub mod loader;

/// A GPS trace: an ordered sequence of (longitude, latitude) points.
#[derive(Debug, Clone)]
pub struct GpsTrace {
    pub id: String,
    pub points: Vec<(f64, f64)>,
}

impl GpsTrace {
    pub fn point_count(&self) -> usize {
        self.points.len()
    }
}
