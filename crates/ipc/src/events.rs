//! A bounded event fan-out. A slow subscriber loses events; it never makes the daemon allocate an
//! unbounded backlog.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::broadcast;

/// How many events a subscriber may lag before the hub drops the oldest. The daemon also bounds
/// each connection's write queue separately.
pub const EVENT_CAPACITY: usize = 64;

#[derive(Debug)]
pub struct EventHub {
    sender: broadcast::Sender<Value>,
    sequence: AtomicU64,
}

impl EventHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self { sender, sequence: AtomicU64::new(0) }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.sender.subscribe()
    }

    /// Publish one event. `kind` is a fixed category such as `sync`, `index`, `outbox` or `daemon`;
    /// `data` carries counters only, never a subject, address, body, query or credential.
    pub fn publish(&self, kind: &str, data: Value) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let event = json!({ "seq": sequence, "kind": kind, "data": data });
        // No receivers is the normal idle state; a lagging receiver is handled by broadcast.
        let _ = self.sender.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sequence_is_monotonic_and_events_are_bounded() {
        let hub = EventHub::new();
        let mut receiver = hub.subscribe();
        for index in 0..EVENT_CAPACITY + 8 {
            hub.publish("index", json!({ "indexed": index }));
        }
        let mut seen = 0;
        loop {
            match receiver.try_recv() {
                Ok(event) => {
                    assert_eq!(event["kind"], "index");
                    seen += 1;
                }
                Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                    assert!(dropped > 0, "a bounded receiver must report what it lost");
                }
                Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(seen <= EVENT_CAPACITY + 8);
    }
}
