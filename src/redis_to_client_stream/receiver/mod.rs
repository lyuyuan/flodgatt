//! Receives data from Redis, sorts it by `ClientAgent`, and stores it until
//! polled by the correct `ClientAgent`.  Also manages sububscriptions and
//! unsubscriptions to/from Redis.
mod message_queues;
use crate::{
    config::{self, RedisInterval},
    pubsub_cmd,
    redis_to_client_stream::redis::{redis_cmd, RedisConn, RedisStream},
};
use futures::{Async, Poll};
pub use message_queues::{MessageQueues, MsgQueue};
use serde_json::Value;
use std::{collections, net, time};
use tokio::io::Error;
use uuid::Uuid;

/// The item that streams from Redis and is polled by the `ClientAgent`
#[derive(Debug)]
pub struct Receiver {
    pub pubsub_connection: RedisStream,
    secondary_redis_connection: net::TcpStream,
    redis_poll_interval: RedisInterval,
    redis_polled_at: time::Instant,
    timeline: String,
    manager_id: Uuid,
    pub msg_queues: MessageQueues,
    clients_per_timeline: collections::HashMap<String, i32>,
}

impl Receiver {
    /// Create a new `Receiver`, with its own Redis connections (but, as yet, no
    /// active subscriptions).
    pub fn new(redis_cfg: config::RedisConfig) -> Self {
        let RedisConn {
            primary: pubsub_connection,
            secondary: secondary_redis_connection,
            namespace: redis_namespace,
            polling_interval: redis_poll_interval,
        } = RedisConn::new(redis_cfg);

        Self {
            pubsub_connection: RedisStream::from_stream(pubsub_connection)
                .with_namespace(redis_namespace),
            secondary_redis_connection,
            redis_poll_interval,
            redis_polled_at: time::Instant::now(),
            timeline: String::new(),
            manager_id: Uuid::default(),
            msg_queues: MessageQueues(collections::HashMap::new()),
            clients_per_timeline: collections::HashMap::new(),
        }
    }

    /// Assigns the `Receiver` a new timeline to monitor and runs other
    /// first-time setup.
    ///
    /// Note: this method calls `subscribe_or_unsubscribe_as_needed`,
    /// so Redis PubSub subscriptions are only updated when a new timeline
    /// comes under management for the first time.
    pub fn manage_new_timeline(&mut self, manager_id: Uuid, timeline: &str) {
        self.manager_id = manager_id;
        self.timeline = timeline.to_string();
        self.msg_queues
            .insert(self.manager_id, MsgQueue::new(timeline));
        self.subscribe_or_unsubscribe_as_needed(timeline);
    }

    /// Set the `Receiver`'s manager_id and target_timeline fields to the appropriate
    /// value to be polled by the current `StreamManager`.
    pub fn configure_for_polling(&mut self, manager_id: Uuid, timeline: &str) {
        self.manager_id = manager_id;
        self.timeline = timeline.to_string();
    }

    /// Drop any PubSub subscriptions that don't have active clients and check
    /// that there's a subscription to the current one.  If there isn't, then
    /// subscribe to it.
    fn subscribe_or_unsubscribe_as_needed(&mut self, timeline: &str) {
        let start_time = std::time::Instant::now();
        let timelines_to_modify = self
            .msg_queues
            .calculate_timelines_to_add_or_drop(timeline.to_string());

        // Record the lower number of clients subscribed to that channel
        for change in timelines_to_modify {
            let count_of_subscribed_clients = self
                .clients_per_timeline
                .entry(change.timeline.clone())
                .and_modify(|n| *n += change.in_subscriber_number)
                .or_insert_with(|| 1);
            // If no clients, unsubscribe from the channel
            if *count_of_subscribed_clients <= 0 {
                pubsub_cmd!("unsubscribe", self, change.timeline.clone());
            } else if *count_of_subscribed_clients == 1 && change.in_subscriber_number == 1 {
                pubsub_cmd!("subscribe", self, change.timeline.clone());
            }
        }
        if start_time.elapsed().as_millis() > 1 {
            log::warn!("Sending cmd to Redis took: {:?}", start_time.elapsed());
        };
    }
}

/// The stream that the ClientAgent polls to learn about new messages.
impl futures::stream::Stream for Receiver {
    type Item = Value;
    type Error = Error;

    /// Returns the oldest message in the `ClientAgent`'s queue (if any).
    ///
    /// Note: This method does **not** poll Redis every time, because polling
    /// Redis is signifiantly more time consuming that simply returning the
    /// message already in a queue.  Thus, we only poll Redis if it has not
    /// been polled lately.
    fn poll(&mut self) -> Poll<Option<Value>, Self::Error> {
        let (timeline, id) = (self.timeline.clone(), self.manager_id);
        if self.redis_polled_at.elapsed() > *self.redis_poll_interval {
            self.pubsub_connection.poll_redis(&mut self.msg_queues);
            self.redis_polled_at = time::Instant::now();
        }

        // Record current time as last polled time
        self.msg_queues.update_time_for_target_queue(id);

        // If the `msg_queue` being polled has any new messages, return the first (oldest) one
        match self.msg_queues.oldest_msg_in_target_queue(id, timeline) {
            Some(value) => Ok(Async::Ready(Some(value))),
            _ => Ok(Async::NotReady),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        pubsub_cmd!("unsubscribe", self, self.timeline.clone());
    }
}