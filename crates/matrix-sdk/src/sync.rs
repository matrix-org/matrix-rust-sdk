use std::time::Duration;

use matrix_sdk_base::{
    deserialized_responses::{JoinedRoom, LeftRoom, SyncResponse},
    instant::Instant,
};
use ruma::api::client::sync::sync_events;
use tracing::{error, warn};

use crate::{event_handler::HandlerKind, Client, Result};

/// Internal functionality related to getting events from the server
/// (`sync_events` endpoint)
impl Client {
    pub(crate) async fn process_sync(
        &self,
        response: sync_events::v3::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().receive_sync_response(response).await?;
        self.handle_sync_response(response).await
    }

    #[tracing::instrument(skip(self, response))]
    pub(crate) async fn handle_sync_response(
        &self,
        response: SyncResponse,
    ) -> Result<SyncResponse> {
        let SyncResponse {
            next_batch: _,
            rooms,
            presence,
            account_data,
            to_device,
            device_lists: _,
            device_one_time_keys_count: _,
            ambiguity_changes: _,
            notifications,
        } = &response;

        self.handle_sync_events(HandlerKind::GlobalAccountData, &None, &account_data.events)
            .await?;
        self.handle_sync_events(HandlerKind::Presence, &None, &presence.events).await?;
        self.handle_sync_events(HandlerKind::ToDevice, &None, &to_device.events).await?;

        for (room_id, room_info) in &rooms.join {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!(%room_id, "Can't call event handler, room not found");
                continue;
            }

            let JoinedRoom { unread_notifications: _, timeline, state, account_data, ephemeral } =
                room_info;

            self.handle_sync_events(HandlerKind::EphemeralRoomData, &room, &ephemeral.events)
                .await?;
            self.handle_sync_events(HandlerKind::RoomAccountData, &room, &account_data.events)
                .await?;
            self.handle_sync_state_events(&room, &state.events).await?;
            self.handle_sync_timeline_events(&room, &timeline.events).await?;
        }

        for (room_id, room_info) in &rooms.leave {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!(%room_id, "Can't call event handler, room not found");
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
                error!(%room_id, "Can't call event handler, room not found");
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

        // Construct notification event handler futures
        let mut futures = Vec::new();
        for handler in &*self.notification_handlers().await {
            for (room_id, room_notifications) in notifications {
                let room = match self.get_room(room_id) {
                    Some(room) => room,
                    None => {
                        warn!(%room_id, "Can't call notification handler, room not found");
                        continue;
                    }
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

        Ok(response)
    }

    async fn sleep() {
        #[cfg(target_arch = "wasm32")]
        let _ = wasm_timer::Delay::new(Duration::from_secs(1)).await;

        #[cfg(not(target_arch = "wasm32"))]
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    pub(crate) async fn sync_loop_helper(
        &self,
        sync_settings: &mut crate::config::SyncSettings<'_>,
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
}
