use crate::context::{MatchContext, MatchResult, MatchRoute};
use futures::Sink;
use routers_shard::ShardId;
use serde::Serialize;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NatsSinkError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("publish failed")]
    Publish,
}

/// Fire-and-forget sink for [`MatchContext`] to `{subject_prefix}.{target_shard}`.
///
/// Uses core NATS publish — no JetStream ack round-trips.
pub fn match_publish_sink<S>(
    nc: async_nats::Client,
    subject_prefix: String,
) -> impl Sink<MatchContext<S>, Error = NatsSinkError>
where
    S: ShardId + fmt::Display + Serialize + Send + 'static,
{
    futures::sink::unfold(nc, move |nc, ctx: MatchContext<S>| {
        let subject = format!("{}.{}", subject_prefix, ctx.target_shard);
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&ctx)
                .map_err(NatsSinkError::Serialize)?
                .into();
            nc.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(nc)
        }
    })
}

pub fn result_sink(
    nc: async_nats::Client,
    subject: String,
) -> impl Sink<MatchResult, Error = NatsSinkError> {
    futures::sink::unfold(nc, move |nc, result: MatchResult| {
        let subject = subject.clone();
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&result)
                .map_err(NatsSinkError::Serialize)?
                .into();
            nc.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(nc)
        }
    })
}

/// Fire-and-forget sink for [`MatchRoute`] to `matched.routes.{vehicle_id}`.
///
/// Subject is `{subject_prefix}.{vehicle_id}`. Callers typically pass
/// `"matched.routes"` as the prefix.
pub fn route_sink(
    nc: async_nats::Client,
    subject_prefix: String,
) -> impl Sink<MatchRoute, Error = NatsSinkError> {
    futures::sink::unfold(nc, move |nc, route: MatchRoute| {
        let subject = format!("{}.{}", subject_prefix, route.vehicle_id);
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&route)
                .map_err(NatsSinkError::Serialize)?
                .into();
            nc.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(nc)
        }
    })
}
