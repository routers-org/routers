//! Element and ProcessedElement iterator, and item definitions

pub mod item;
pub mod iterator;
pub mod processed_iterator;

#[doc(hidden)]
pub mod variants;

#[doc(inline)]
pub use item::Element;
#[doc(inline)]
pub use item::ProcessedElement;
#[doc(inline)]
pub use iterator::ElementIterator;
#[doc(inline)]
pub use processed_iterator::ProcessedElementIterator;
#[doc(inline)]
pub use variants::OsmEntryId;
#[doc(inline)]
pub use variants::common::*;
