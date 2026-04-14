// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Types for live location sharing.
//!
//! Live location sharing allows users to share their real-time location with
//! others in a room via [MSC3489](https://github.com/matrix-org/matrix-spec-proposals/pull/3489).

use std::sync::Arc;

use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};
use imbl::Vector;
use matrix_sdk_base::{deserialized_responses::SyncOrStrippedState, event_cache::Event};
use matrix_sdk_common::locks::Mutex;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncStateEvent,
        beacon::OriginalSyncBeaconEvent,
        beacon_info::{BeaconInfoEventContent, OriginalSyncBeaconInfoEvent},
        location::LocationContent,
        relation::RelationType,
    },
};

use super::Room;
use crate::event_handler::EventHandlerDropGuard;

/// Details of the last known location beacon.
#[derive(Clone, Debug)]
pub struct LastLocation {
    /// The most recent location content of the asset.
    pub location: LocationContent,
    /// The timestamp of when the location was updated.
    pub ts: MilliSecondsSinceUnixEpoch,
}

/// Details of a user's live location share.
#[derive(Clone, Debug)]
pub struct LiveLocationShare {
    /// The user ID of the person sharing their live location.
    pub user_id: OwnedUserId,
    /// The asset's last known location, if any beacon has been received.
    pub last_location: Option<LastLocation>,
    /// The event ID of the beacon_info state event for this share.
    pub beacon_id: OwnedEventId,
    /// Information about the associated beacon event.
    pub beacon_info: BeaconInfoEventContent,
}

/// Tracks active live location shares in a room using an [`ObservableVector`].
///
/// Registers event handlers for beacon (location update) and beacon info
/// (share started/stopped) events and reflects changes into a vector that
/// callers can subscribe to via [`LiveLocationShares::subscribe`].
///
/// Event handlers are automatically unregistered when this struct is dropped.
#[derive(Debug)]
pub struct LiveLocationShares {
    shares: Arc<Mutex<ObservableVector<LiveLocationShare>>>,
    _beacon_guard: EventHandlerDropGuard,
    _beacon_info_guard: EventHandlerDropGuard,
}

impl LiveLocationShares {
    /// Create a new [`LiveLocationShares`] for the given room.
    ///
    /// Loads the current active shares from the event cache as initial state,
    /// then begins listening for beacon events to keep the vector up-to-date.
    pub(super) async fn new(room: Room) -> Self {
        let mut shares = ObservableVector::new();
        let initial_shares =
            Self::get_initial_live_location_shares(&room).await.unwrap_or_default();
        shares.append(initial_shares);
        let shares = Arc::new(Mutex::new(shares));

        let beacon_handle = room.add_event_handler({
            let shares = shares.clone();
            async move |event: OriginalSyncBeaconEvent| {
                Self::handle_beacon_event(&shares, event);
            }
        });
        let beacon_guard = room.client.event_handler_drop_guard(beacon_handle);
        let beacon_info_handle = room.add_event_handler({
            let shares = shares.clone();
            async move |event: OriginalSyncBeaconInfoEvent, room: Room| {
                Self::handle_beacon_info_event(&shares, &room, event).await;
            }
        });
        let beacon_info_guard = room.client.event_handler_drop_guard(beacon_info_handle);
        Self { shares, _beacon_guard: beacon_guard, _beacon_info_guard: beacon_info_guard }
    }

    /// Subscribe to changes and updates in the live location shares.
    ///
    /// Returns a snapshot of the current items alongside a batched stream of
    /// [`eyeball_im::VectorDiff`]s that describe subsequent changes.
    pub fn subscribe(
        &self,
    ) -> (Vector<LiveLocationShare>, VectorSubscriberBatchedStream<LiveLocationShare>) {
        self.shares.lock().subscribe().into_values_and_batched_stream()
    }

    /// Get all currently active live location shares in a room.
    async fn get_initial_live_location_shares(
        room: &Room,
    ) -> crate::Result<Vector<LiveLocationShare>> {
        // Beacon infos are stored in the state store, not the event cache.
        let beacon_infos = room.get_state_events_static::<BeaconInfoEventContent>().await?;
        // Event cache is only needed for finding last location (optional).
        let event_cache = room.event_cache().await.ok();
        let mut shares = Vector::new();
        for raw_beacon_info in beacon_infos {
            let Ok(event) = raw_beacon_info.deserialize() else { continue };
            let Some((user_id, beacon_info, event_id)) = Self::extract_live_beacon_info(event)
            else {
                continue;
            };
            let last_location = match &event_cache {
                Some((cache, _drop_handles)) => Self::find_last_location(cache, &event_id).await,
                None => None,
            };
            shares.push_back(LiveLocationShare {
                user_id,
                beacon_info,
                beacon_id: event_id,
                last_location,
            });
        }
        Ok(shares)
    }

    /// Extracts a live beacon info from a state event.
    ///
    /// Returns `(user_id, content, event_id)`, or `None` if the event is
    /// redacted/stripped or not currently live.
    fn extract_live_beacon_info(
        event: SyncOrStrippedState<BeaconInfoEventContent>,
    ) -> Option<(OwnedUserId, BeaconInfoEventContent, OwnedEventId)> {
        let SyncOrStrippedState::Sync(SyncStateEvent::Original(ev)) = event else {
            return None;
        };
        if !ev.content.is_live() {
            return None;
        }
        Some((ev.state_key, ev.content, ev.event_id))
    }

    /// Finds the most recent beacon event referencing the given beacon_info
    /// event.
    ///
    /// Beacon events use an `m.reference` relation to point to their
    /// originating `beacon_info` state event. The event cache's relation
    /// index lets us look them up directly by ID without scanning all
    /// cached events.
    async fn find_last_location(
        cache: &crate::event_cache::RoomEventCache,
        beacon_info_event_id: &OwnedEventId,
    ) -> Option<LastLocation> {
        cache
            .find_event_relations(beacon_info_event_id, Some(vec![RelationType::Reference]))
            .await
            .ok()?
            .into_iter()
            .rev()
            .find_map(|e| Self::event_to_last_location(&e))
    }

    /// Converts an [`Event`] to a [`LastLocation`] if it is a beacon event.
    fn event_to_last_location(event: &Event) -> Option<LastLocation> {
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Beacon(
            beacon_event,
        ))) = event.kind.raw().deserialize()
        {
            beacon_event.as_original().map(|beacon| LastLocation {
                location: beacon.content.location.clone(),
                ts: beacon.origin_server_ts,
            })
        } else {
            None
        }
    }

    /// Handles a single beacon event (location update).
    ///
    /// Matches the beacon to its share via `relates_to.event_id`, which
    /// references the originating `beacon_info` state event.
    fn handle_beacon_event(
        shares: &Mutex<ObservableVector<LiveLocationShare>>,
        event: OriginalSyncBeaconEvent,
    ) {
        let beacon_info_event_id = &event.content.relates_to.event_id;
        let mut shares = shares.lock();
        if let Some(idx) = shares.iter().position(|s| s.beacon_id == *beacon_info_event_id) {
            // Check if beacon info is still live, if not, remove the share and ignore the
            // beacon event.
            let mut share = shares[idx].clone();
            if !share.beacon_info.is_live() {
                shares.remove(idx);
                return;
            }
            let last_location =
                LastLocation { location: event.content.location, ts: event.origin_server_ts };
            share.last_location = Some(last_location);
            shares.set(idx, share);
        }
    }

    /// Handles a single beacon_info state event (share started or stopped).
    ///
    /// When a new beacon info is received for an already tracked user, the
    /// share is removed from the vector. If the new beacon info is live, we add
    /// it at the end of the vector, looking up the event cache to find any
    /// beacon event that may have arrived before the beacon_info.
    async fn handle_beacon_info_event(
        shares: &Mutex<ObservableVector<LiveLocationShare>>,
        room: &Room,
        event: OriginalSyncBeaconInfoEvent,
    ) {
        {
            let mut shares = shares.lock();
            if let Some(idx) = shares.iter().position(|s| s.user_id == *event.state_key) {
                shares.remove(idx);
            }
        }
        if event.content.is_live() {
            let last_location = if let Ok((cache, _drop_handles)) = room.event_cache().await {
                Self::find_last_location(&cache, &event.event_id).await
            } else {
                None
            };
            let share = LiveLocationShare {
                user_id: event.state_key,
                beacon_id: event.event_id,
                beacon_info: event.content,
                last_location,
            };
            shares.lock().push_back(share);
        }
    }
}
