// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! The SDK's representation of the result of a `/sync` request.

use std::{
    collections::{BTreeMap, btree_map},
    fmt,
    time::Duration,
};

pub use matrix_sdk_base::sync::*;
use matrix_sdk_base::{
    debug::{
        DebugInvitedRoom, DebugKnockedRoom, DebugListOfProcessedToDeviceEvents,
        DebugListOfRawEventsNoId,
    },
    sleep::sleep,
    sync::SyncResponse as BaseSyncResponse,
    timer,
};
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use ruma::{
    OwnedRoomId, RoomId,
    api::client::sync::sync_events::{
        self,
        v3::{InvitedRoom, KnockedRoom},
    },
    events::{AnyGlobalAccountDataEvent, presence::PresenceEvent},
    serde::Raw,
    time::Instant,
};
use tracing::{debug, error, instrument, warn};

use crate::{Client, Result, Room, event_handler::HandlerKind};

/// The processed response of a `/sync` request.
#[derive(Clone, Default)]
pub struct SyncResponse {
    /// The batch token to supply in the `since` param of the next `/sync`
    /// request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: RoomUpdates,
    /// Updates to the presence status of other users.
    pub presence: Vec<Raw<PresenceEvent>>,
    /// The global private data created by this user.
    pub account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Messages sent directly between devices.
    pub to_device: Vec<ProcessedToDeviceEvent>,
    /// New notifications per room.
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

impl SyncResponse {
    pub(crate) fn new(next_batch: String, base_response: BaseSyncResponse) -> Self {
        let BaseSyncResponse { rooms, presence, account_data, to_device, notifications } =
            base_response;

        Self { next_batch, rooms, presence, account_data, to_device, notifications }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncResponse")
            .field("next_batch", &self.next_batch)
            .field("rooms", &self.rooms)
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("to_device", &DebugListOfProcessedToDeviceEvents(&self.to_device))
            .field("notifications", &self.notifications)
            .finish_non_exhaustive()
    }
}

/// A batch of updates to a room.
#[derive(Clone)]
pub enum RoomUpdate {
    /// Updates to a room the user is no longer in.
    Left {
        /// Room object with general information on the room.
        room: Room,
        /// Updates to the room.
        updates: LeftRoomUpdate,
    },
    /// Updates to a room the user is currently in.
    Joined {
        /// Room object with general information on the room.
        room: Room,
        /// Updates to the room.
        updates: JoinedRoomUpdate,
    },
    /// Updates to a room the user is invited to.
    Invited {
        /// Room object with general information on the room.
        room: Room,
        /// Updates to the room.
        updates: InvitedRoom,
    },
    /// Updates to a room the user knocked on.
    Knocked {
        /// Room object with general information on the room.
        room: Room,
        /// Updates to the room.
        updates: KnockedRoom,
    },
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RoomUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left { room, updates } => {
                f.debug_struct("Left").field("room", room).field("updates", updates).finish()
            }
            Self::Joined { room, updates } => {
                f.debug_struct("Joined").field("room", room).field("updates", updates).finish()
            }
            Self::Invited { room, updates } => f
                .debug_struct("Invited")
                .field("room", room)
                .field("updates", &DebugInvitedRoom(updates))
                .finish(),
            Self::Knocked { room, updates } => f
                .debug_struct("Knocked")
                .field("room", room)
                .field("updates", &DebugKnockedRoom(updates))
                .finish(),
        }
    }
}

/// Internal functionality related to getting events from the server
/// (`sync_events` endpoint)
impl Client {
    /// Receive a sync response, compute extra information out of it and store
    /// the interesting bits in the database, then call all the handlers.
    pub(crate) async fn process_sync(
        &self,
        response: sync_events::v3::Response,
    ) -> Result<BaseSyncResponse> {
        subscribe_to_room_latest_events(
            self,
            response.rooms.join.keys().chain(response.rooms.leave.keys()),
        )
        .await;

        let response = Box::pin(self.base_client().receive_sync_response(response)).await?;

        // Some new keys might have been received, so trigger a backup if needed.
        #[cfg(feature = "e2e-encryption")]
        self.encryption().backups().maybe_trigger_backup();

        self.call_sync_response_handlers(&response).await?;

        Ok(response)
    }

    /// Calls event handlers and notification handlers after a sync response has
    /// been processed.
    ///
    /// At this point, the sync response's data has been taken into account and
    /// persisted in the store, if needs be. This function is only calling
    /// the event, room update and notification handlers.
    #[tracing::instrument(skip(self, response))]
    pub(crate) async fn call_sync_response_handlers(
        &self,
        response: &BaseSyncResponse,
    ) -> Result<()> {
        let _timer = timer!(tracing::Level::TRACE, "_method");

        let BaseSyncResponse { rooms, presence, account_data, to_device, notifications } = response;

        let now = Instant::now();
        self.handle_sync_events(HandlerKind::GlobalAccountData, None, account_data).await?;
        self.handle_sync_events(HandlerKind::Presence, None, presence).await?;
        self.handle_sync_to_device_events(to_device).await?;

        // Ignore errors when there are no receivers.
        let _ = self.inner.room_updates_sender.send(rooms.clone());

        for (room_id, room_info) in &rooms.joined {
            let Some(room) = self.get_room(room_id) else {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            };

            self.send_room_update(room_id, || RoomUpdate::Joined {
                room: room.clone(),
                updates: room_info.clone(),
            });

            let JoinedRoomUpdate {
                unread_notifications: _,
                timeline,
                state,
                account_data,
                ephemeral,
                ambiguity_changes: _,
            } = room_info;

            let room = Some(&room);
            self.handle_sync_events(HandlerKind::RoomAccountData, room, account_data).await?;
            self.handle_sync_state_events(room, state).await?;
            self.handle_sync_timeline_events(room, &timeline.events).await?;
            // Handle ephemeral events after timeline, read receipts in here
            // could refer to timeline events from the same response.
            self.handle_sync_events(HandlerKind::EphemeralRoomData, room, ephemeral).await?;
        }

        for (room_id, room_info) in &rooms.left {
            let Some(room) = self.get_room(room_id) else {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            };

            self.send_room_update(room_id, || RoomUpdate::Left {
                room: room.clone(),
                updates: room_info.clone(),
            });

            let LeftRoomUpdate { timeline, state, account_data, ambiguity_changes: _ } = room_info;

            let room = Some(&room);
            self.handle_sync_events(HandlerKind::RoomAccountData, room, account_data).await?;
            self.handle_sync_state_events(room, state).await?;
            self.handle_sync_timeline_events(room, &timeline.events).await?;
        }

        for (room_id, room_info) in &rooms.invited {
            let Some(room) = self.get_room(room_id) else {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            };

            self.send_room_update(room_id, || RoomUpdate::Invited {
                room: room.clone(),
                updates: room_info.clone(),
            });

            let invite_state = &room_info.invite_state.events;
            self.handle_sync_events(HandlerKind::StrippedState, Some(&room), invite_state).await?;
        }

        for (room_id, room_info) in &rooms.knocked {
            let Some(room) = self.get_room(room_id) else {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            };

            self.send_room_update(room_id, || RoomUpdate::Knocked {
                room: room.clone(),
                updates: room_info.clone(),
            });

            let knock_state = &room_info.knock_state.events;
            self.handle_sync_events(HandlerKind::StrippedState, Some(&room), knock_state).await?;
        }

        debug!("Ran event handlers in {:?}", now.elapsed());

        let now = Instant::now();

        // Construct notification event handler futures
        let mut futures = Vec::new();
        for handler in &*self.notification_handlers().await {
            for (room_id, room_notifications) in notifications {
                let Some(room) = self.get_room(room_id) else {
                    warn!(?room_id, "Can't call notification handler, room not found");
                    continue;
                };

                futures.extend(room_notifications.iter().map(|notification| {
                    (handler)(notification.clone(), room.clone(), self.clone())
                }));
            }
        }

        // Run the notification handler futures with the
        // `self.notification_handlers` lock no longer being held, in order.
        for fut in futures {
            fut.await;
        }

        debug!("Ran notification handlers in {:?}", now.elapsed());

        Ok(())
    }

    fn send_room_update(&self, room_id: &RoomId, make_msg: impl FnOnce() -> RoomUpdate) {
        if let btree_map::Entry::Occupied(entry) =
            self.inner.room_update_channels.lock().unwrap().entry(room_id.to_owned())
        {
            let tx = entry.get();
            if tx.receiver_count() == 0 {
                entry.remove();
            } else {
                _ = tx.send(make_msg());
            }
        }
    }

    async fn sleep() {
        sleep(Duration::from_secs(1)).await;
    }

    pub(crate) async fn sync_loop_helper(
        &self,
        sync_settings: &mut crate::config::SyncSettings,
    ) -> Result<SyncResponse> {
        let response = self.sync_once(sync_settings.clone()).await;

        match response {
            Ok(r) => {
                sync_settings.token = r.next_batch.clone().into();
                Ok(r)
            }
            Err(e) => {
                error!("Received an invalid response: {e}");
                Err(e)
            }
        }
    }

    pub(crate) async fn delay_sync(last_sync_time: &mut Option<Instant>) {
        let now = Instant::now();

        // If the last sync happened less than a second ago, sleep for a
        // while to not hammer out requests if the server doesn't respect
        // the sync timeout.
        if let Some(t) = last_sync_time
            && now - *t <= Duration::from_secs(1)
        {
            Self::sleep().await;
        }

        *last_sync_time = Some(now);
    }
}

/// Call `LatestEvents::listen_to_room` for rooms in `response`.
///
/// That way, the latest event is computed and updated for all rooms receiving
/// an update from the sync.
#[instrument(skip_all)]
pub(crate) async fn subscribe_to_room_latest_events<'a, R>(client: &'a Client, room_ids: R)
where
    R: Iterator<Item = &'a OwnedRoomId>,
{
    if !client.event_cache().has_subscribed() {
        return;
    }

    let latest_events = client.latest_events().await;

    for room_id in room_ids {
        if let Err(error) = latest_events.listen_to_room(room_id).await {
            error!(?error, ?room_id, "Failed to listen to the latest event for this room");
        }
    }
}
