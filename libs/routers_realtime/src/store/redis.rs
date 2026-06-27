use std::fmt::format;

use thiserror::Error;
use url::Url;

use crate::store::Storable;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serialisation: {0}")]
    Serialisation(#[from] postcard::Error),
}

type Result<T> = std::result::Result<T, StoreError>;

pub struct RedisStore<T: Storable> {
    conn: redis::aio::MultiplexedConnection,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Storable> RedisStore<T> {
    pub async fn new(url: Url) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;

        Ok(Self {
            conn,
            _phantom: std::marker::PhantomData,
        })
    }
}

impl<T: Storable> RedisStore<T> {
    pub async fn write_many(&mut self, batch: &[T], limit: usize) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let mut pipe = redis::pipe();

        for item in batch {
            let value = postcard::to_allocvec(item)?;
            let key = format!("vehicle:{}:positions", item.key());

            pipe.cmd("XADD")
                .arg(key)
                .arg("MAXLEN")
                .arg("~")
                .arg(limit)
                .arg("*")
                .arg("shard")
                .arg(item.shard_id().to_string())
                .arg("val")
                .arg(value)
                .ignore();
        }

        let _: () = pipe.query_async(&mut self.conn).await?;
        Ok(())
    }
}
