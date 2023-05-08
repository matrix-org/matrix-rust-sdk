//! The SDK's representation of the result of a `/sync` request.

use std::{collections::BTreeMap, time::Duration};

use eyeball::unique::Observable;
pub use matrix_sdk_base::sync::*;
use matrix_sdk_base::{
    deserialized_responses::AmbiguityChanges, instant::Instant,
    sync::SyncResponse as BaseSyncResponse,
};
use ruma::{
    api::client::{
        push::get_notifications::v3::Notification,
        sync::sync_events::{self, v3::Presence, DeviceLists},
    },
    events::{AnyGlobalAccountDataEvent, AnyToDeviceEvent},
    serde::Raw,
    DeviceKeyAlgorithm, OwnedRoomId, RoomId,
};
use tracing::{debug, error, warn};

use crate::{event_handler::HandlerKind, Client, Result};

/// The processed response of a `/sync` request.
#[derive(Clone, Debug, Default)]
pub struct SyncResponse {
    /// The batch token to supply in the `since` param of the next `/sync`
    /// request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Presence,
    /// The global private data created by this user.
    pub account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Messages sent directly between devices.
    pub to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    /// Information on E2E device updates.
    ///
    /// Only present on an incremental sync.
    pub device_lists: DeviceLists,
    /// For each key algorithm, the number of unclaimed one-time keys
    /// currently held on the server for a device.
    pub device_one_time_keys_count: BTreeMap<DeviceKeyAlgorithm, u64>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
    /// New notifications per room.
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

impl SyncResponse {
    pub(crate) fn new(next_batch: String, base_response: BaseSyncResponse) -> Self {
        let BaseSyncResponse {
            rooms,
            presence,
            account_data,
            to_device_events,
            device_lists,
            device_one_time_keys_count,
            ambiguity_changes,
            notifications,
        } = base_response;

        Self {
            next_batch,
            rooms,
            presence,
            account_data,
            to_device_events,
            device_lists,
            device_one_time_keys_count,
            ambiguity_changes,
            notifications,
        }
    }
}

/// Internal functionality related to getting events from the server
/// (`sync_events` endpoint)
impl Client {
    pub(crate) async fn process_sync(
        &self,
        response: sync_events::v3::Response,
    ) -> Result<BaseSyncResponse> {
        let response = Box::pin(self.base_client().receive_sync_response(response)).await?;
        self.handle_sync_response(&response).await?;
        Ok(response)
    }

    #[tracing::instrument(skip(self, response))]
    pub(crate) async fn handle_sync_response(&self, response: &BaseSyncResponse) -> Result<()> {
        let BaseSyncResponse {
            rooms,
            presence,
            account_data,
            to_device_events,
            device_lists: _,
            device_one_time_keys_count: _,
            ambiguity_changes: _,
            notifications,
        } = response;

        let now = Instant::now();
        self.handle_sync_events(HandlerKind::GlobalAccountData, &None, account_data).await?;
        self.handle_sync_events(HandlerKind::Presence, &None, &presence.events).await?;
        self.handle_sync_events(HandlerKind::ToDevice, &None, to_device_events).await?;

        for (room_id, room_info) in &rooms.join {
            if room_info.timeline.limited {
                self.notify_sync_gap(room_id);
            }

            let room = self.get_room(room_id);
            if room.is_none() {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            }

            let JoinedRoom { unread_notifications: _, timeline, state, account_data, ephemeral } =
                room_info;

            self.handle_sync_events(HandlerKind::RoomAccountData, &room, account_data).await?;
            self.handle_sync_state_events(&room, &state.events).await?;
            self.handle_sync_timeline_events(&room, &timeline.events).await?;
            // Handle ephemeral events after timeline, read receipts in here
            // could refer to timeline events from the same response.
            self.handle_sync_events(HandlerKind::EphemeralRoomData, &room, &ephemeral.events)
                .await?;
        }

        for (room_id, room_info) in &rooms.leave {
            if room_info.timeline.limited {
                self.notify_sync_gap(room_id);
            }

            let room = self.get_room(room_id);
            if room.is_none() {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            }

            let LeftRoom { timeline, state, account_data } = room_info;

            self.handle_sync_events(HandlerKind::RoomAccountData, &room, &account_data.events)
                .await?;
            self.handle_sync_state_events(&room, &state.events).await?;
            self.handle_sync_timeline_events(&room, &timeline.events).await?;
        }

        for (room_id, room_info) in &rooms.invite {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!(?room_id, "Can't call event handler, room not found");
                continue;
            }

            // FIXME: Destructure room_info
            self.handle_sync_events(
                HandlerKind::StrippedState,
                &room,
                &room_info.invite_state.events,
            )
            .await?;
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

    async fn sleep() {
        #[cfg(target_arch = "wasm32")]
        gloo_timers::future::TimeoutFuture::new(1_000).await;

        #[cfg(not(target_arch = "wasm32"))]
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    pub(crate) async fn sync_loop_helper(
        &self,
        sync_settings: &mut crate::config::SyncSettings,
    ) -> Result<SyncResponse> {
        let response = self.sync_once(sync_settings.clone()).await;

        match response {
            Ok(r) => {
                sync_settings.token = Some(r.next_batch.clone());
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
        if let Some(t) = last_sync_time {
            if now - *t <= Duration::from_secs(1) {
                Self::sleep().await;
            }
        }

        *last_sync_time = Some(now);
    }

    fn notify_sync_gap(&self, room_id: &RoomId) {
        let mut lock = self.inner.sync_gap_broadcast_txs.lock().unwrap();
        if let Some(tx) = lock.get_mut(room_id) {
            Observable::set(tx, ());
        }
    }
}
