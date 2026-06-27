pub mod event;

use anyhow::{Context, Result};
use futures::stream::StreamExt;
use lapin::message::Delivery;
use lapin::options::ExchangeDeclareOptions;
use lapin::types::ShortString;
use lapin::{
    BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, Consumer,
    ExchangeKind,
    options::{BasicConsumeOptions, BasicPublishOptions, QueueBindOptions, QueueDeclareOptions},
    types::FieldTable,
};

pub mod bus;

pub struct Topic {
    connection: Connection,
    consumer: Consumer,
    channel: Channel,
    opts: TopicOpts,
}

pub struct TopicOpts {
    exchange_name: ShortString,
    routing_key: ShortString,
    queue_name: ShortString,
    consumer_tag: ShortString,
}

impl Default for TopicOpts {
    fn default() -> Self {
        Self {
            exchange_name: ShortString::from("amqprs.example"),
            routing_key: ShortString::from("amq.topic"),
            queue_name: ShortString::from("queue"),
            consumer_tag: ShortString::from("amq.consumer"),
        }
    }
}

impl TopicOpts {
    pub fn with_queue(self, queue_name: &str) -> Self {
        Self {
            queue_name: ShortString::from(queue_name),
            ..self
        }
    }
}

impl Topic {
    pub async fn finish(&self) -> Result<()> {
        self.connection.close(0, "".into()).await?;
        Ok(())
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

    pub async fn recv(&mut self) -> Result<Delivery> {
        // Await the next message from the stream
        let delivery = self
            .consumer
            .next()
            .await
            .context("Consumer Closed")?
            .context("Error in consumer stream")?;

        Ok(delivery)
    }

    pub async fn new(opts: TopicOpts) -> Result<Self> {
        let uri = "amqp://127.0.0.1:5672/%2f";

        let connection = Connection::connect(uri, ConnectionProperties::default())
            .await
            .context("Cannot connect to AMQP")?;

        let channel = connection.create_channel().await.unwrap();

        channel
            .exchange_declare(
                opts.exchange_name.clone(),
                ExchangeKind::Topic,
                ExchangeDeclareOptions::default(),
                FieldTable::default(),
            )
            .await
            .context("Cannot declare AMQP exchange")?;

        // Passing an empty string instructs RabbitMQ to auto-generate a queue name.
        // We set exclusive: true so the queue is deleted when the connection closes.
        let queue = channel
            .queue_declare(
                opts.queue_name.clone(),
                QueueDeclareOptions {
                    exclusive: true,
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
}
