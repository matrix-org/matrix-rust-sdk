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

//! Storage usage reporting and selective clearing of the client's stores, for
//! a "manage storage" screen.

use matrix_sdk_base::{
    StateChanges,
    event_cache::store::{EventCacheStoreLockGuard, EventCacheStoreLockState},
};
pub use matrix_sdk_common::storage_usage::StorageUsage;
use ruma::{OwnedMxcUri, OwnedRoomId, time::SystemTime};
use tracing::{info, instrument, warn};

use crate::{Client, Result};

/// How much storage each of the client's caches uses, overall and per room.
///
/// See [`Client::storage_usage`].
#[derive(Clone, Debug, Default)]
pub struct StorageUsageReport {
    /// The room keys (megolm inbound group sessions) of the crypto store.
    pub room_keys: StorageUsage,
    /// The room data of the state store: room infos, state events, members,
    /// profiles, receipts, display names, room account data.
    pub room_state: StorageUsage,
    /// The events of the event cache store.
    pub events: StorageUsage,
    /// The media contents of the media store; a room's media are the contents
    /// referenced by its stored media messages, thumbnails included.
    pub media: StorageUsage,
}

/// One room's share of each cache, in bytes. See
/// [`Client::storage_usage_by_room`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoomStorageUsage {
    /// The room keys (megolm inbound group sessions) of the crypto store.
    pub room_keys_bytes: u64,
    /// The room's data in the state store.
    pub room_state_bytes: u64,
    /// The room's events in the event cache store.
    pub events_bytes: u64,
    /// The media contents referenced by the room's stored media messages.
    pub media_bytes: u64,
}

impl RoomStorageUsage {
    fn total(&self) -> u64 {
        self.room_keys_bytes + self.room_state_bytes + self.events_bytes + self.media_bytes
    }
}

impl Client {
    /// Walk the known rooms' storage usage, calling `on_room` for each room
    /// with any cached data, as soon as its numbers are known: the cheap
    /// per-room stores (keys, state, events) are measured up front in a few
    /// grouped queries, then each room's media (which requires decoding its
    /// media messages to find their URIs) is measured in turn, biggest rooms
    /// first, so that a UI can fill in progressively.
    #[instrument(skip_all)]
    pub async fn storage_usage_by_room(
        &self,
        mut on_room: impl FnMut(OwnedRoomId, RoomStorageUsage),
    ) -> Result<()> {
        let room_ids: Vec<OwnedRoomId> =
            self.rooms().iter().map(|room| room.room_id().to_owned()).collect();

        #[cfg(feature = "e2e-encryption")]
        let room_keys = match self.olm_machine().await.as_ref() {
            Some(olm_machine) => olm_machine.store().room_keys_storage_usage(&room_ids).await?,
            None => StorageUsage::default(),
        };
        #[cfg(not(feature = "e2e-encryption"))]
        let room_keys = StorageUsage::default();
        let room_state = self.state_store().storage_usage(&room_ids).await?;
        let events = self.locked_event_cache_store().await?.storage_usage(&room_ids).await?;

        let mut usages: Vec<(OwnedRoomId, RoomStorageUsage)> = room_ids
            .into_iter()
            .map(|room_id| {
                let usage = RoomStorageUsage {
                    room_keys_bytes: room_keys.per_room.get(&room_id).copied().unwrap_or(0),
                    room_state_bytes: room_state.per_room.get(&room_id).copied().unwrap_or(0),
                    events_bytes: events.per_room.get(&room_id).copied().unwrap_or(0),
                    media_bytes: 0,
                };
                (room_id, usage)
            })
            .collect();
        usages.sort_by_key(|(_, usage)| std::cmp::Reverse(usage.total()));

        for (room_id, mut usage) in usages {
            // A room without events has no cached media messages to attribute
            // media to.
            if usage.events_bytes > 0 {
                let uris = self.locked_event_cache_store().await?.room_media_uris(&room_id).await?;
                if !uris.is_empty() {
                    let room_media = [(room_id.clone(), uris)];
                    let media = self.media_store().lock().await?.storage_usage(&room_media).await?;
                    usage.media_bytes = media.per_room.get(&room_id).copied().unwrap_or(0);
                }
            }

            if usage.total() > 0 {
                on_room(room_id, usage);
            }
        }

        Ok(())
    }

    /// Measure how much storage each of the caches uses, overall and per
    /// known room.
    ///
    /// Sizes are the stored payloads' sizes, an approximation of the space
    /// taken on disk. Stores that can't measure themselves report zero.
    #[instrument(skip(self))]
    pub async fn storage_usage(&self) -> Result<StorageUsageReport> {
        let room_ids: Vec<OwnedRoomId> =
            self.rooms().iter().map(|room| room.room_id().to_owned()).collect();

        let (events, room_media) = {
            let store = self.locked_event_cache_store().await?;
            let events = store.storage_usage(&room_ids).await?;

            let mut room_media = Vec::with_capacity(room_ids.len());
            for room_id in &room_ids {
                let uris = store.room_media_uris(room_id).await?;
                if !uris.is_empty() {
                    room_media.push((room_id.clone(), uris));
                }
            }

            (events, room_media)
        };

        let media = self.media_store().lock().await?.storage_usage(&room_media).await?;
        let room_state = self.state_store().storage_usage(&room_ids).await?;

        #[cfg(feature = "e2e-encryption")]
        let room_keys = match self.olm_machine().await.as_ref() {
            Some(olm_machine) => olm_machine.store().room_keys_storage_usage(&room_ids).await?,
            None => StorageUsage::default(),
        };
        #[cfg(not(feature = "e2e-encryption"))]
        let room_keys = StorageUsage::default();

        Ok(StorageUsageReport { room_keys, room_state, events, media })
    }

    /// Delete the room keys (megolm inbound group sessions) of the given rooms,
    /// or of all rooms.
    ///
    /// Encrypted history can't be read again without a key backup to fetch the
    /// keys from; the keys are downloaded again from the backup on demand.
    #[instrument(skip(self))]
    pub async fn clear_room_keys(&self, room_ids: Option<&[OwnedRoomId]>) -> Result<()> {
        #[cfg(feature = "e2e-encryption")]
        if let Some(olm_machine) = self.olm_machine().await.as_ref() {
            olm_machine.store().remove_inbound_group_sessions(room_ids).await?;
            info!("Room keys cleared");
        }
        Ok(())
    }

    /// Clear the given rooms' cached data: their events (the event cache) and
    /// their members, profiles, receipts and display names (the bulk of their
    /// state store data, fetched again lazily). The rooms stay known, with
    /// their room info and state events.
    #[instrument(skip(self))]
    pub async fn clear_room_caches(&self, room_ids: &[OwnedRoomId]) -> Result<()> {
        let mut changes = StateChanges::default();

        for room_id in room_ids {
            if let Err(error) = self.event_cache().clear_room(room_id).await {
                warn!(?room_id, "Failed to clear the room's event cache: {error}");
            }

            self.state_store().remove_room_members(room_id).await?;

            if let Some(room) = self.get_room(room_id) {
                room.mark_members_missing();
                changes.add_room(room.clone_info());
            }
        }

        // Persist the members-missing flag, so that the members are fetched
        // again after a restart too.
        self.state_store().save_changes(&changes).await?;
        info!(num_rooms = room_ids.len(), "Room caches cleared");

        Ok(())
    }

    /// Delete the cached media contents of the given rooms (the contents
    /// referenced by their stored media messages), or all the cached media;
    /// only those last accessed before the given time, when given.
    #[instrument(skip(self))]
    pub async fn clear_media_cache(
        &self,
        room_ids: Option<&[OwnedRoomId]>,
        last_accessed_before: Option<SystemTime>,
    ) -> Result<()> {
        let uris = match room_ids {
            Some(room_ids) => {
                let store = self.locked_event_cache_store().await?;
                let mut uris: Vec<OwnedMxcUri> = Vec::new();
                for room_id in room_ids {
                    uris.extend(store.room_media_uris(room_id).await?);
                }
                Some(uris)
            }
            None => None,
        };

        self.media_store()
            .lock()
            .await?
            .remove_media_contents(uris.as_deref(), last_accessed_before)
            .await?;
        info!(
            num_rooms = room_ids.map(<[_]>::len),
            num_uris = uris.as_ref().map(Vec::len),
            ?last_accessed_before,
            "Media cache cleared"
        );

        Ok(())
    }
}

impl Client {
    /// The event cache store, locked; whether the lock is dirty doesn't
    /// matter for the read-only queries and the deletions above (the event
    /// cache reloads its memory from the store on its own).
    async fn locked_event_cache_store(&self) -> Result<EventCacheStoreLockGuard> {
        Ok(match self.event_cache_store().lock().await? {
            EventCacheStoreLockState::Clean(guard) | EventCacheStoreLockState::Dirty(guard) => {
                guard
            }
        })
    }
}
