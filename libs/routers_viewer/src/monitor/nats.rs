use std::sync::mpsc::Sender;

use async_nats::jetstream;
use futures::StreamExt;
use routers_realtime::context::{MatchContext, MatchResult, MatchRoute};
use routers_shard::Geohash;

pub(crate) enum InboundMessage {
    Context(MatchContext<Geohash>),
    Result(MatchResult),
    Route(MatchRoute),
}

pub(crate) async fn subscribe(
    url: String,
    tx: Sender<InboundMessage>,
    egui_ctx: egui::Context,
) -> anyhow::Result<()> {
    let client = async_nats::connect(&url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect to {url}: {e}"))?;
    let js = jetstream::new(client.clone());

    let ctx_tx = tx.clone();
    let ctx_egui = egui_ctx.clone();
    let context_task = tokio::spawn(subscribe_contexts(js, ctx_tx, ctx_egui));

    let res_tx = tx.clone();
    let res_egui = egui_ctx.clone();
    let result_task = tokio::spawn(subscribe_results(client.clone(), res_tx, res_egui));

    let route_egui = egui_ctx.clone();
    let route_task = tokio::spawn(subscribe_routes(client, tx, route_egui));

    tokio::try_join!(
        async { context_task.await.map_err(|e| anyhow::anyhow!("context task panicked: {e}"))? },
        async { result_task.await.map_err(|e| anyhow::anyhow!("result task panicked: {e}"))? },
        async { route_task.await.map_err(|e| anyhow::anyhow!("route task panicked: {e}"))? },
    )?;

    Ok(())
}

async fn subscribe_contexts(
    js: jetstream::Context,
    tx: Sender<InboundMessage>,
    egui_ctx: egui::Context,
) -> anyhow::Result<()> {
    let stream = js
        .get_stream("MATCH")
        .await
        .map_err(|e| anyhow::anyhow!("get MATCH stream: {e}"))?;

    let consumer = stream
        .create_consumer(jetstream::consumer::pull::Config {
            filter_subject: "match.>".to_owned(),
            ack_policy: jetstream::consumer::AckPolicy::None,
            deliver_policy: jetstream::consumer::DeliverPolicy::New,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("create context consumer: {e}"))?;

    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("context message stream: {e}"))?;

    while let Some(Ok(msg)) = messages.next().await {
        if let Ok(ctx) = postcard::from_bytes::<MatchContext<Geohash>>(&msg.payload) {
            if tx.send(InboundMessage::Context(ctx)).is_err() {
                break;
            }
            egui_ctx.request_repaint();
        }
    }

    Ok(())
}

async fn subscribe_results(
    client: async_nats::Client,
    tx: Sender<InboundMessage>,
    egui_ctx: egui::Context,
) -> anyhow::Result<()> {
    let mut sub = client
        .subscribe("matched.positions")
        .await
        .map_err(|e| anyhow::anyhow!("subscribe matched.positions: {e}"))?;

    while let Some(msg) = sub.next().await {
        if let Ok(result) = postcard::from_bytes::<MatchResult>(&msg.payload) {
            if tx.send(InboundMessage::Result(result)).is_err() {
                break;
            }
            egui_ctx.request_repaint();
        }
    }

    Ok(())
}

async fn subscribe_routes(
    client: async_nats::Client,
    tx: Sender<InboundMessage>,
    egui_ctx: egui::Context,
) -> anyhow::Result<()> {
    let mut sub = client
        .subscribe("matched.routes.>")
        .await
        .map_err(|e| anyhow::anyhow!("subscribe matched.routes.>: {e}"))?;

    while let Some(msg) = sub.next().await {
        if let Ok(route) = postcard::from_bytes::<MatchRoute>(&msg.payload) {
            if tx.send(InboundMessage::Route(route)).is_err() {
                break;
            }
            egui_ctx.request_repaint();
        }
    }

    Ok(())
}

