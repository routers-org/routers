use core::num::NonZeroU8;

pub enum Lanes {
    Bidirectional(NonZeroU8),
    Unidirectional {
        forward: Option<NonZeroU8>,
        backward: Option<NonZeroU8>,
    },
}
