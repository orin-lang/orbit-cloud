//! Queues behind a trait. [`MessageQueue`] is everything orbitd needs from a queue backend: publishing (from the API
//! and from apps through `orbit:queue/v1`), leasing for consumers and the operator views. The built-in backend is the
//! SQLite store (`impl MessageQueue for Store`); a NATS JetStream backend can be added by implementing the trait and
//! passing it to [`crate::executor::Executor::with_queues`]. NATS is not implemented.
//!
//! Queue names are node-wide (not per app): an app that publishes `orders` feeds the consumer of `orders`, and the
//! spec's `publish` list is the grant. When projects land, the project becomes a prefix applied here.

use std::sync::Arc;

use oc_store::{Message, NackOutcome, QueueStats, Result, Store, StoreError};
use orbit_caps::{QueueBackend, QueueError};
use tokio::sync::Notify;

/// A message queue backend.
pub trait MessageQueue: Send + Sync + 'static {
    /// Appends a message; returns its id.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad queue name; backend failures.
    fn publish(&self, queue: &str, body: &[u8]) -> Result<i64>;

    /// Appends messages in order, atomically if the backend can; returns the first id.
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn publish_batch(&self, queue: &str, bodies: &[Vec<u8>]) -> Result<i64>;

    /// Leases the oldest deliverable message for `lease_ms`.
    ///
    /// # Errors
    /// Backend failures.
    fn lease(&self, queue: &str, lease_ms: i64) -> Result<Option<Message>>;

    /// Acknowledges a delivery.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the lease is no longer current.
    fn ack(&self, id: i64, token: &str) -> Result<()>;

    /// Rejects a delivery: retry after `delay_ms` or dead-letter at `max_attempts`.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the lease is no longer current.
    fn nack(
        &self,
        id: i64,
        token: &str,
        error: &str,
        delay_ms: i64,
        max_attempts: i64,
    ) -> Result<NackOutcome>;

    /// Counts per queue.
    ///
    /// # Errors
    /// Backend failures.
    fn stats(&self) -> Result<Vec<QueueStats>>;

    /// Dead letters of a queue, oldest first.
    ///
    /// # Errors
    /// Backend failures.
    fn dead_letters(&self, queue: &str, limit: i64) -> Result<Vec<Message>>;

    /// Makes the dead letters of a queue ready again; returns how many.
    ///
    /// # Errors
    /// Backend failures.
    fn redrive(&self, queue: &str) -> Result<usize>;
}

impl MessageQueue for Store {
    fn publish(&self, queue: &str, body: &[u8]) -> Result<i64> {
        self.enqueue(queue, body)
    }
    fn publish_batch(&self, queue: &str, bodies: &[Vec<u8>]) -> Result<i64> {
        self.enqueue_batch(queue, bodies)
    }
    fn lease(&self, queue: &str, lease_ms: i64) -> Result<Option<Message>> {
        Store::lease(self, queue, lease_ms)
    }
    fn ack(&self, id: i64, token: &str) -> Result<()> {
        Store::ack(self, id, token)
    }
    fn nack(
        &self,
        id: i64,
        token: &str,
        error: &str,
        delay_ms: i64,
        max_attempts: i64,
    ) -> Result<NackOutcome> {
        Store::nack(self, id, token, error, delay_ms, max_attempts)
    }
    fn stats(&self) -> Result<Vec<QueueStats>> {
        self.queues()
    }
    fn dead_letters(&self, queue: &str, limit: i64) -> Result<Vec<Message>> {
        Store::dead_letters(self, queue, limit)
    }
    fn redrive(&self, queue: &str) -> Result<usize> {
        Store::redrive(self, queue)
    }
}

/// `orbit:queue/v1` over a [`MessageQueue`]; wakes idle consumers after every publish.
pub struct GuestQueues {
    queues: Arc<dyn MessageQueue>,
    signal: Arc<Notify>,
}

impl GuestQueues {
    /// An adapter publishing into `queues` and notifying `signal`.
    #[must_use]
    pub fn new(queues: Arc<dyn MessageQueue>, signal: Arc<Notify>) -> Self {
        Self { queues, signal }
    }
}

fn map(e: StoreError) -> QueueError {
    QueueError::Backend(e.to_string())
}

impl QueueBackend for GuestQueues {
    // The tenant (the app name) is not part of the queue identity: queues are node-wide and the app's `publish` grant
    // decides which it may feed.
    fn publish(
        &self,
        _tenant: &str,
        queue: &str,
        body: &[u8],
    ) -> std::result::Result<(), QueueError> {
        self.queues.publish(queue, body).map_err(map)?;
        self.signal.notify_waiters();
        Ok(())
    }

    fn publish_batch(
        &self,
        _tenant: &str,
        queue: &str,
        bodies: &[Vec<u8>],
    ) -> std::result::Result<(), QueueError> {
        self.queues.publish_batch(queue, bodies).map_err(map)?;
        self.signal.notify_waiters();
        Ok(())
    }
}
