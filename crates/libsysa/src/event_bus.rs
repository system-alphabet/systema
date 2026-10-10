use std::collections::HashMap;
use std::sync::Arc;

/// Topics that can be subscribed to on the event bus.
///
/// Workers no longer emit free-form event strings; all runtime state
/// changes flow through the unified `unit.state_update` protocol, which
/// SysA re-dispatches as [`EventTopic::UnitStateChange`].
///
/// Control-bus subscribers (System Wrapper bridge flavors) additionally
/// receive the richer life-cycle topics below; their `data` payloads are
/// protobuf-encoded control-plane messages.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum EventTopic {
    /// A unit's runtime state changed (unified `unit.state_update`).
    UnitStateChange,
    /// A unit's full snapshot changed (data = encoded `UnitSnapshot`).
    UnitChanged,
    /// A unit was loaded into memory (data = encoded `UnitSnapshot`).
    UnitNew,
    /// A unit was unloaded from memory (data = encoded `UnitRemovedEvent`).
    UnitRemoved,
    /// New cgroup metrics arrived (data = encoded `UnitCgroupMetrics`).
    UnitMetrics,
    /// A job was created (data = encoded `JobEvent`, result empty).
    JobNew,
    /// A job finished (data = encoded `JobEvent`, result filled).
    JobCompleted,
    /// Events for a single, named unit.  Used by subscribers that want to
    /// watch a specific unit instead of every unit.
    Unit(String),
    /// Subscribe to **all** topics.
    All,
}

/// An event published on the event bus.
#[derive(Debug, Clone)]
pub struct Event {
    pub topic: EventTopic,
    pub unit_name: String,
    pub worker_id: String,
    pub timestamp: tokio::time::Instant,
    pub data: bytes::Bytes,
}

/// A subscriber that receives events from the event bus.
#[async_trait::async_trait]
pub trait EventSubscriber: Send + Sync {
    /// Return the list of topics this subscriber is interested in.
    /// Return `[EventTopic::All]` to receive every event.
    fn topics(&self) -> Vec<EventTopic>;

    /// Called for every matching event.
    /// Implementations should not block for long — spawn a task if necessary.
    async fn on_event(&self, event: &Event);
}

struct SubscriberHandle {
    id: u64,
    subscriber: Arc<dyn EventSubscriber>,
}

/// A lightweight in-process publish–subscribe event bus.
///
/// Producers call [`dispatch`](EventBus::dispatch); subscribers registered
/// via [`subscribe`](EventBus::subscribe) receive matching events.
///
/// # Concurrency
///
/// `EventBus` is `Send + Sync` and is designed to be held behind
/// `Arc<RwLock<EventBus>>`.  Dispatching holds a read lock; subscribing /
/// unsubscribing requires a write lock.
pub struct EventBus {
    subscribers: HashMap<EventTopic, Vec<SubscriberHandle>>,
    all_subscribers: Vec<SubscriberHandle>,
    next_id: u64,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            subscribers: HashMap::new(),
            all_subscribers: Vec::new(),
            next_id: 1,
        }
    }

    /// Register a subscriber.  Returns a unique subscriber ID that can be
    /// passed to [`unsubscribe`](EventBus::unsubscribe).
    pub fn subscribe(&mut self, subscriber: Arc<dyn EventSubscriber>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let handle = SubscriberHandle { id, subscriber };

        let topics = handle.subscriber.topics();
        for topic in &topics {
            if *topic == EventTopic::All {
                self.all_subscribers.push(SubscriberHandle {
                    id: handle.id,
                    subscriber: handle.subscriber.clone(),
                });
            } else {
                self.subscribers
                    .entry(topic.clone())
                    .or_default()
                    .push(SubscriberHandle {
                        id: handle.id,
                        subscriber: handle.subscriber.clone(),
                    });
            }
        }

        id
    }

    /// Remove a previously registered subscriber by its ID.
    /// Returns `true` if the subscriber was found and removed.
    pub fn unsubscribe(&mut self, id: u64) -> bool {
        let topic_count_before: usize = self.subscribers.values().map(|v| v.len()).sum();
        for handles in self.subscribers.values_mut() {
            handles.retain(|h| h.id != id);
        }
        self.subscribers.retain(|_, handles| !handles.is_empty());
        let topic_count_after: usize = self.subscribers.values().map(|v| v.len()).sum();

        let all_before = self.all_subscribers.len();
        self.all_subscribers.retain(|h| h.id != id);
        let all_after = self.all_subscribers.len();

        (topic_count_before != topic_count_after) || (all_before != all_after)
    }

    /// Dispatch `event` to every matching subscriber.
    ///
    /// Subscribers are called **sequentially** in registration order.
    /// If you need concurrent delivery, use [`dispatch_async`](EventBus::dispatch_async).
    pub async fn dispatch(&self, event: &Event) {
        if let Some(handles) = self.subscribers.get(&event.topic) {
            for handle in handles {
                handle.subscriber.on_event(event).await;
            }
        }

        if let Some(handles) = self
            .subscribers
            .get(&EventTopic::Unit(event.unit_name.clone()))
        {
            for handle in handles {
                handle.subscriber.on_event(event).await;
            }
        }

        for handle in &self.all_subscribers {
            handle.subscriber.on_event(event).await;
        }
    }

    /// Dispatch `event` to every matching subscriber concurrently, spawning
    /// a Tokio task per subscriber so that a slow handler cannot block others.
    pub async fn dispatch_async(&self, event: &Event) {
        let mut tasks = Vec::new();

        if let Some(handles) = self.subscribers.get(&event.topic) {
            for handle in handles {
                let sub = handle.subscriber.clone();
                let ev = event.clone();
                tasks.push(tokio::spawn(async move {
                    sub.on_event(&ev).await;
                }));
            }
        }

        if let Some(handles) = self
            .subscribers
            .get(&EventTopic::Unit(event.unit_name.clone()))
        {
            for handle in handles {
                let sub = handle.subscriber.clone();
                let ev = event.clone();
                tasks.push(tokio::spawn(async move {
                    sub.on_event(&ev).await;
                }));
            }
        }

        for handle in &self.all_subscribers {
            let sub = handle.subscriber.clone();
            let ev = event.clone();
            tasks.push(tokio::spawn(async move {
                sub.on_event(&ev).await;
            }));
        }

        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSubscriber {
        topic: EventTopic,
        received: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl EventSubscriber for RecordingSubscriber {
        fn topics(&self) -> Vec<EventTopic> {
            vec![self.topic.clone()]
        }

        async fn on_event(&self, event: &Event) {
            self.received.lock().await.push(event.unit_name.clone());
        }
    }

    fn unit_event(unit_name: &str) -> Event {
        Event {
            topic: EventTopic::UnitStateChange,
            unit_name: unit_name.to_string(),
            worker_id: "worker".to_string(),
            timestamp: tokio::time::Instant::now(),
            data: bytes::Bytes::new(),
        }
    }

    #[tokio::test]
    async fn unit_topic_receives_only_matching_unit() {
        let mut bus = EventBus::new();
        let received = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        bus.subscribe(Arc::new(RecordingSubscriber {
            topic: EventTopic::Unit("svc-a.service".to_string()),
            received: received.clone(),
        }));

        bus.dispatch(&unit_event("svc-a.service")).await;
        bus.dispatch(&unit_event("svc-b.service")).await;

        assert_eq!(*received.lock().await, vec!["svc-a.service"]);
    }
}
