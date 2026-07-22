mod nats;
mod trace;

pub use nats::NATSSink;
pub use nats::NATSStream;
pub use trace::{last_sent_at, span_between, wallclock};
