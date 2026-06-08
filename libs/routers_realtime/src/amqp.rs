use anyhow::{Context, Result};
use lapin::{
    BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, Consumer, ExchangeKind,
    options::{
        BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::{FieldTable, ShortString},
};
use std::pin::Pin;
use std::task::{self, Poll};

pub struct Topic {
    connection: Connection,
    consumer: Consumer,
    channel: Channel,
    opts: TopicOpts,
}

pub struct TopicOpts {
    pub url: String,
    exchange_name: ShortString,
    routing_key: ShortString,
    queue_name: ShortString,
    pub consumer_tag: ShortString,
    exclusive: bool,
    auto_delete: bool,
    /// Per-consumer prefetch limit (0 = unlimited). Set to a reasonable value
    /// (e.g. 100–500) to prevent RabbitMQ from flooding the consumer buffer.
    pub prefetch: u16,
}

impl Default for TopicOpts {
    fn default() -> Self {
        Self {
            url: "amqp://127.0.0.1:5672/%2f".into(),
            exchange_name: ShortString::from("amqprs.example"),
            routing_key: ShortString::from("amq.topic"),
            queue_name: ShortString::from("queue"),
            consumer_tag: ShortString::from("amq.consumer"),
            exclusive: false,
            auto_delete: false,
            prefetch: 0,
        }
    }
}

impl TopicOpts {
    pub fn from_env() -> Self {
        let prefetch: u16 = std::env::var("AMQP_PREFETCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Self {
            url: std::env::var("RABBITMQ_URL")
                .unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into()),
            exchange_name: ShortString::from(
                std::env::var("RABBITMQ_EXCHANGE")
                    .unwrap_or_else(|_| "amqprs.example".into())
                    .as_str(),
            ),
            routing_key: ShortString::from(
                std::env::var("RABBITMQ_ROUTING_KEY")
                    .unwrap_or_else(|_| "amq.topic".into())
                    .as_str(),
            ),
            queue_name: ShortString::from(
                std::env::var("RABBITMQ_QUEUE")
                    .unwrap_or_else(|_| "queue".into())
                    .as_str(),
            ),
            consumer_tag: ShortString::from("amq.consumer"),
            exclusive: false,
            auto_delete: false,
            prefetch,
        }
    }

    pub fn with_queue(self, queue_name: &str) -> Self {
        Self {
            queue_name: ShortString::from(queue_name),
            ..self
        }
    }

    /// Mark the queue as auto-delete: it will be removed when the last consumer disconnects.
    /// Use this for ephemeral producer queues (e.g. replay senders) so they clean up on exit.
    pub fn with_auto_delete(self) -> Self {
        Self {
            auto_delete: true,
            ..self
        }
    }
}

impl Topic {
    pub async fn new(opts: TopicOpts) -> Result<Self> {
        let connection = Connection::connect(&opts.url, ConnectionProperties::default())
            .await
            .context("Cannot connect to AMQP")?;

        let channel = connection.create_channel().await.unwrap();

        if opts.prefetch > 0 {
            channel
                .basic_qos(opts.prefetch, BasicQosOptions::default())
                .await
                .context("Cannot set AMQP prefetch")?;
        }

        channel
            .exchange_declare(
                opts.exchange_name.clone(),
                ExchangeKind::Topic,
                ExchangeDeclareOptions::default(),
                FieldTable::default(),
            )
            .await
            .context("Cannot declare AMQP exchange")?;

        let queue = channel
            .queue_declare(
                opts.queue_name.clone(),
                QueueDeclareOptions {
                    exclusive: opts.exclusive,
                    auto_delete: opts.auto_delete,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .context("Cannot declare AMQP queue")?;

        channel
            .queue_bind(
                queue.name().clone(),
                opts.exchange_name.clone(),
                opts.routing_key.clone(),
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await?;

        let consumer = channel
            .basic_consume(
                opts.queue_name.clone(),
                opts.consumer_tag.clone(),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await?;

        Ok(Topic {
            consumer,
            connection,
            channel,
            opts,
        })
    }

    pub async fn send(&self, content: &[u8]) -> Result<Confirmation> {
        let confirmation = self
            .channel
            .basic_publish(
                self.opts.exchange_name.clone(),
                self.opts.routing_key.clone(),
                BasicPublishOptions::default(),
                content,
                BasicProperties::default(),
            )
            .await?
            .await?;
        Ok(confirmation)
    }

    pub async fn finish(&self) -> Result<()> {
        self.connection.close(0, "".into()).await?;
        Ok(())
    }
}

impl futures::Stream for Topic {
    type Item = Result<lapin::message::Delivery, lapin::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.consumer).poll_next(cx)
    }
}
