// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{sync::Arc, time::Duration};

use matrix_sdk_base::sticky::{
    RemovalReason, StickyEvent as SdkStickyEvent, StickyEventsUpdate as SdkStickyEventsUpdate,
    StickyKey as SdkStickyKey,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use tokio::sync::broadcast::error::RecvError;

use super::Room;
use crate::{
    TaskHandle, encryption::EventEncryptionInfo, error::ClientError, runtime::get_runtime_handle,
};

/// The key under which a sticky event is tracked in a room.
///
/// A room holds at most one live sticky event per key.
#[derive(Clone, uniffi::Record)]
pub struct StickyKey {
    /// The sender of the event.
    pub sender: String,
    /// The type of the event, e.g. `m.rtc.member`.
    pub event_type: String,
    /// The `content.sticky_key` of the event.
    pub sticky_key: String,
}

impl From<SdkStickyKey> for StickyKey {
    fn from(key: SdkStickyKey) -> Self {
        Self {
            sender: key.sender.to_string(),
            event_type: key.event_type.to_string(),
            sticky_key: key.sticky_key,
        }
    }
}

/// A sticky event that is currently live in a room.
#[derive(Clone, uniffi::Record)]
pub struct StickyEvent {
    /// The key under which the event is tracked.
    pub key: StickyKey,
    /// The event ID.
    pub event_id: String,
    /// When the event stops being sticky, in milliseconds since the Unix epoch.
    pub expires_at_ms: u64,
    /// The event as a JSON string, decrypted if it was encrypted.
    pub event_json: String,
    /// The encryption info of the event, if it was encrypted.
    pub encryption_info: Option<EventEncryptionInfo>,
}

impl From<SdkStickyEvent> for StickyEvent {
    fn from(event: SdkStickyEvent) -> Self {
        let event_json = event.raw().json().get().to_owned();
        let encryption_info = event.encryption_info().map(|info| info.as_ref().into());

        Self {
            key: event.key.into(),
            event_id: event.event_id.to_string(),
            expires_at_ms: event.expires_at.0.into(),
            event_json,
            encryption_info,
        }
    }
}

/// A sticky event that is no longer live in a room.
#[derive(Clone, uniffi::Record)]
pub struct StickyEventRemoval {
    /// The key the event was tracked under.
    pub key: StickyKey,
    /// Why it is no longer live.
    pub reason: RemovalReason,
}

/// A change to the sticky events of a room, as delivered to a
/// [`StickyEventsListener`].
///
/// Consumers can use these updates to maintain a map of the live sticky events
/// keyed by [`StickyKey`].
#[derive(Clone, uniffi::Enum)]
pub enum StickyEventsUpdate {
    /// A full replacement of the map.
    Reset {
        /// Every sticky event that is currently live.
        events: Vec<StickyEvent>,
    },
    /// An incremental change.
    Changes {
        /// Events that appeared under a key that had no live event.
        added: Vec<StickyEvent>,
        /// Events that replaced the live event of their key.
        updated: Vec<StickyEvent>,
        /// Keys whose live event disappeared.
        removed: Vec<StickyEventRemoval>,
    },
}

impl From<SdkStickyEventsUpdate> for StickyEventsUpdate {
    fn from(update: SdkStickyEventsUpdate) -> Self {
        Self::Changes {
            added: update.added.into_iter().map(Into::into).collect(),
            updated: update.updated.into_iter().map(Into::into).collect(),
            removed: update
                .removed
                .into_iter()
                .map(|(key, reason)| StickyEventRemoval { key: key.into(), reason })
                .collect(),
        }
    }
}

/// A listener for the sticky events of a room.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait StickyEventsListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, update: StickyEventsUpdate);
}

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// The sticky events that are currently live in this room.
    pub fn sticky_events(&self) -> Vec<StickyEvent> {
        self.inner.sticky_events().live().into_iter().map(Into::into).collect()
    }

    /// Subscribe to the sticky events of this room.
    ///
    /// The listener first receives a [`StickyEventsUpdate::Reset`] with the
    /// sticky events that are currently live, then a
    /// [`StickyEventsUpdate::Changes`] for every change. Should it fall behind
    /// and miss changes, it receives another [`StickyEventsUpdate::Reset`] to
    /// catch up with.
    pub fn subscribe_to_sticky_events(
        self: Arc<Self>,
        listener: Box<dyn StickyEventsListener>,
    ) -> Arc<TaskHandle> {
        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            // Subscribe before taking the snapshot, so that no change slips
            // between the two. A change that made it into the snapshot _and_ is
            // delivered afterwards is harmless, as consumers apply changes by
            // key.
            let mut subscriber = self.inner.sticky_events().subscribe();

            listener.on_update(StickyEventsUpdate::Reset { events: self.sticky_events() });

            loop {
                match subscriber.recv().await {
                    Ok(update) => listener.on_update(update.into()),
                    // The channel doesn't replay what was missed, so start over
                    // from the live set.
                    Err(RecvError::Lagged(_)) => {
                        listener
                            .on_update(StickyEventsUpdate::Reset { events: self.sticky_events() });
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })))
    }

    /// Send a sticky event to this room.
    //
    /// Note that if the homeserver doesn't support sticky events, it will
    /// ignore the duration and send the event unsticky. Server support can
    /// be checked with [`Client::is_sticky_events_supported`].
    ///
    /// # Arguments
    ///
    /// - `event_type` - The type of the event to send.
    /// - `content` - The content of the event to send encoded as JSON string.
    /// - `duration_ms` - How long the event stays sticky for, in milliseconds,
    ///   clamped to one hour.
    ///
    /// # Returns
    ///
    /// The event ID of the newly sent event.
    ///
    /// [`Client::is_sticky_events_supported`]: crate::client::Client::is_sticky_events_supported
    pub async fn send_sticky_raw(
        &self,
        event_type: String,
        content: String,
        duration_ms: u64,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| matrix_sdk::Error::SerdeJson(e))?;

        let response = self
            .inner
            .send_raw(&event_type, content_json)
            .with_sticky_duration(Duration::from_millis(duration_ms))
            .await?;

        Ok(response.response.event_id.to_string())
    }
}
