mod expansion;

#[doc(hidden)]
pub mod all_compute;
#[doc(hidden)]
pub mod methods;
#[doc(hidden)]
pub mod selective;
#[doc(hidden)]
pub mod variant;

#[doc(inline)]
pub use all_compute::*;
#[doc(inline)]
pub use methods::*;
#[doc(inline)]
pub use selective::*;
#[doc(inline)]
pub use variant::*;
