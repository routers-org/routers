pub mod access_tag;
pub mod primitives;
pub mod speed_limit;

pub use access_tag::Access;
pub use speed_limit::SpeedLimit;

pub trait Parser: Sized {
    fn parse(tags: &crate::osm::Tags<'_>) -> Option<Self>;
}
