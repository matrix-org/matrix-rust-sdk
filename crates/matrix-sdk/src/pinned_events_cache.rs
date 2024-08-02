use std::{collections::BTreeMap, sync::Arc};

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{EventId, OwnedEventId};
use tokio::sync::RwLock;

/// Cache used to store the events associated with pinned event ids.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone, Debug)]
pub struct PinnedEventCache {
    inner: Arc<InnerPinnedEventCache>,
}

impl PinnedEventCache {
    /// Creates a new empty pinned events cache.
    pub fn new() -> Self {
        Self { inner: Arc::new(InnerPinnedEventCache { events: Default::default() }) }
    }

    /// Gets the event associated with the provided event id, if it exists in
    /// the cache.
    pub async fn get(&self, event_id: &EventId) -> Option<SyncTimelineEvent> {
        let cache = self.inner.events.read().await;
        cache.get(event_id).cloned()
    }

    /// Adds a list of pinned events to the cache in a performant way.
    pub async fn set_bulk(&self, events: &Vec<SyncTimelineEvent>) {
        let mut cache = self.inner.events.write().await;
        for ev in events {
            if let Some(ev_id) = ev.event_id() {
                cache.insert(ev_id.to_owned(), ev.clone());
            }
        }
    }
}

impl Default for PinnedEventCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The non-cloneable implementation of the cache.
#[derive(Debug)]
struct InnerPinnedEventCache {
    /// The pinned events.
    events: RwLock<BTreeMap<OwnedEventId, SyncTimelineEvent>>,
}
