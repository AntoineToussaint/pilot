use crate::Event;
use tokio::sync::broadcast;

/// Capacity of the event bus. Old events are dropped if a consumer is slow.
const BUS_CAPACITY: usize = 512;

/// A clonable handle for producing events.
#[derive(Clone)]
pub struct EventProducer {
    tx: broadcast::Sender<Event>,
}

impl EventProducer {
    pub fn send(&self, event: Event) {
        // Ignore error (means no active consumers).
        let _ = self.tx.send(event);
    }
}

/// A handle for consuming events.
pub struct EventConsumer {
    rx: broadcast::Receiver<Event>,
}

impl EventConsumer {
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.rx.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Event consumer lagged, dropped {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Create a linked producer/consumer pair. Call `subscribe()` on the producer
/// to get additional consumers.
pub fn event_bus() -> (EventProducer, EventConsumer) {
    let (tx, rx) = broadcast::channel(BUS_CAPACITY);
    (EventProducer { tx: tx.clone() }, EventConsumer { rx })
}

impl EventProducer {
    /// Create an additional consumer for this bus.
    pub fn subscribe(&self) -> EventConsumer {
        EventConsumer {
            rx: self.tx.subscribe(),
        }
    }
}
