use serde::{Serialize, de::DeserializeOwned};

mod redis;
pub use redis::RedisStore;

pub trait Storable: Serialize + DeserializeOwned {
    type ShardId: std::fmt::Display;
    type Key: std::fmt::Display;

    fn shard_id(&self) -> Self::ShardId;
    fn key(&self) -> Self::Key;
}
