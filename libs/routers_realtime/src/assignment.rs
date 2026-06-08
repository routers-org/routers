use routers_shard::ShardId;
use std::marker::PhantomData;

pub trait ShardAssignment<S: ShardId> {
    #[allow(async_fn_in_trait)]
    async fn acquire(&self) -> S;
}

pub struct EnvAssignment<S, F> {
    var: &'static str,
    parse: F,
    _marker: PhantomData<S>,
}

impl<S, F> EnvAssignment<S, F> {
    pub fn new(var: &'static str, parse: F) -> Self {
        Self {
            var,
            parse,
            _marker: PhantomData,
        }
    }
}

impl<S, F> ShardAssignment<S> for EnvAssignment<S, F>
where
    S: ShardId,
    F: Fn(&str) -> S,
{
    async fn acquire(&self) -> S {
        let val = std::env::var(self.var)
            .unwrap_or_else(|_| panic!("{} must be set", self.var));
        (self.parse)(&val)
    }
}

#[cfg(not(debug_assertions))]
pub use nats_kv::NatsKvAssignment;

#[cfg(not(debug_assertions))]
mod nats_kv {
    use super::*;

    pub struct NatsKvAssignment {
        store: async_nats::jetstream::kv::Store,
    }

    impl NatsKvAssignment {
        pub async fn new(
            js: async_nats::jetstream::Context,
            bucket: &str,
        ) -> Result<Self, async_nats::Error> {
            let store = js.get_key_value(bucket).await?;
            Ok(Self { store })
        }
    }

    impl<S: ShardId> ShardAssignment<S> for NatsKvAssignment {
        async fn acquire(&self) -> S {
            todo!("NATS KV lease acquisition")
        }
    }
}
