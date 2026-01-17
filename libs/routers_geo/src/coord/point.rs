use core::fmt::Display;
use geo::Coord;
use strum::{EnumCount, IntoEnumIterator, VariantArray};

pub trait FeatureKey: IntoEnumIterator + VariantArray + EnumCount + Display + Copy {}

/// A generic trait used for tiling a point
/// using the `MVT` schema.
pub trait TileItem<T: Clone>: Into<geo::Point> + Clone {
    type Key: FeatureKey;
    const SIZE: usize = <Self::Key as EnumCount>::COUNT;

    /// Returns the identifier of the location pointed to.
    /// This has different representative meaning, according
    /// to where it is located, and what it represents.
    fn id(&self) -> u64 {
        let as_point = Into::<geo::Point>::into(self.clone());
        let hash = geohash::encode(Coord::from(as_point), 8);

        match hash {
            Ok(hash) => crate::cluster::geohash_to_u64(&hash).unwrap_or(0u64),
            Err(_) => 0u64,
        }
    }

    /// Until `generic_const_exprs` is merged (#76560)
    /// this will remain a vectored implementation as size is unknown
    fn entries(&self) -> Vec<(Self::Key, T)>;

    fn values(&self) -> Vec<T> {
        self.entries()
            .iter()
            .map(|(_, value)| value)
            .cloned()
            .collect()
    }

    fn keys() -> &'static [Self::Key] {
        Self::Key::VARIANTS
    }
}
