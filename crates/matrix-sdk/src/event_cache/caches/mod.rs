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

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    ThreadingSupport,
    event_cache::{Event, store::EventCacheStoreLock},
};
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::{broadcast::Sender, mpsc};

use super::{EventCacheError, Result};
use crate::{client::WeakClient, event_cache::EventsOrigin};

pub mod event_linked_chunk;
pub(super) mod lock;
pub mod pagination;
pub mod pinned_events;
pub mod room;
pub mod thread;

/// A type to hold all the caches for a given room.
#[derive(Clone)]
pub struct Caches {
    pub room: room::RoomEventCache,
}

impl Caches {
    /// Create a new [`Caches`].
    pub(super) async fn new(
        weak_client: &WeakClient,
        room_id: &RoomId,
        generic_update_sender: Sender<room::RoomEventCacheGenericUpdate>,
        linked_chunk_update_sender: Sender<room::RoomEventCacheLinkedChunkUpdate>,
        auto_shrink_sender: mpsc::Sender<OwnedRoomId>,
        store: EventCacheStoreLock,
    ) -> Result<Self> {
        let Some(client) = weak_client.get() else {
            return Err(EventCacheError::ClientDropped);
        };

        let pagination_status =
            SharedObservable::new(pagination::PaginationStatus::Idle { hit_timeline_start: false });

        let room = client
            .get_room(room_id)
            .ok_or_else(|| EventCacheError::RoomNotFound { room_id: room_id.to_owned() })?;
        let room_version_rules = room.clone_info().room_version_rules_or_default();

        let enabled_thread_support =
            matches!(client.base_client().threading_support, ThreadingSupport::Enabled { .. });

        let update_sender = room::RoomEventCacheUpdateSender::new(generic_update_sender.clone());

        let own_user_id =
            client.user_id().expect("the user must be logged in, at this point").to_owned();

        let room_state = room::RoomEventCacheStateLock::new(
            own_user_id,
            room_id.to_owned(),
            room_version_rules,
            enabled_thread_support,
            update_sender.clone(),
            linked_chunk_update_sender,
            store,
            pagination_status.clone(),
        )
        .await?;

        let timeline_is_not_empty =
            room_state.read().await?.room_linked_chunk().revents().next().is_some();

        let room_event_cache = room::RoomEventCache::new(
            weak_client.clone(),
            room_state,
            pagination_status,
            room_id.to_owned(),
            auto_shrink_sender,
            update_sender,
        );

        // If at least one event has been loaded, it means there is a timeline. Let's
        // emit a generic update.
        if timeline_is_not_empty {
            let _ = generic_update_sender
                .send(room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });
        }

        Ok(Self { room: room_event_cache })
    }
}

/// A diff update for an event cache timeline represented as a vector.
#[derive(Clone, Debug)]
pub struct TimelineVectorDiffs {
    /// New vector diff for the thread timeline.
    pub diffs: Vec<VectorDiff<Event>>,
    /// The origin that triggered this update.
    pub origin: EventsOrigin,
}
