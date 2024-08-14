use std::collections::BTreeMap;

use imbl::Vector;
use matrix_sdk_base::{sliding_sync::http, sync::SyncResponse, PreviousEventsProvider};
use ruma::{events::AnyToDeviceEvent, serde::Raw, OwnedRoomId};
use tracing::error;

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{Client, Result, SlidingSyncRoom};

impl Client {
    /// Create a [`SlidingSyncBuilder`] tied to this client, with the given
    /// identifier.
    ///
    /// Note: the identifier must not be more than 16 chars long!
    pub fn sliding_sync(&self, id: impl Into<String>) -> Result<SlidingSyncBuilder> {
        Ok(SlidingSync::builder(id.into(), self.clone())?)
    }

    /// Handle all the information provided in a sliding sync response, except
    /// for the e2ee bits.
    ///
    /// If you need to handle encryption too, use the internal
    /// `SlidingSyncResponseProcessor` instead.
    #[cfg(any(test, feature = "testing"))]
    #[tracing::instrument(skip(self, response))]
    pub async fn process_sliding_sync_test_helper(
        &self,
        response: &http::Response,
    ) -> Result<SyncResponse> {
        let response = self
            .base_client()
            .process_sliding_sync(response, &(), self.is_simplified_sliding_sync_enabled())
            .await?;

        tracing::debug!("done processing on base_client");
        self.call_sync_response_handlers(&response).await?;

        Ok(response)
    }
}

struct SlidingSyncPreviousEventsProvider<'a>(&'a BTreeMap<OwnedRoomId, SlidingSyncRoom>);

impl<'a> PreviousEventsProvider for SlidingSyncPreviousEventsProvider<'a> {
    fn for_room(
        &self,
        room_id: &ruma::RoomId,
    ) -> Vector<matrix_sdk_common::deserialized_responses::SyncTimelineEvent> {
        self.0.get(room_id).map(|room| room.timeline_queue()).unwrap_or_default()
    }
}

/// Small helper to handle a `SlidingSync` response's sub parts.
///
/// This will properly handle the encryption and the room response
/// independently, if needs be, making sure that both are properly processed by
/// event handlers.
#[must_use]
pub(crate) struct SlidingSyncResponseProcessor<'a> {
    client: Client,
    to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    response: Option<SyncResponse>,
    rooms: &'a BTreeMap<OwnedRoomId, SlidingSyncRoom>,
}

impl<'a> SlidingSyncResponseProcessor<'a> {
    pub fn new(client: Client, rooms: &'a BTreeMap<OwnedRoomId, SlidingSyncRoom>) -> Self {
        Self { client, to_device_events: Vec::new(), response: None, rooms }
    }

    #[cfg(feature = "e2e-encryption")]
    pub async fn handle_encryption(
        &mut self,
        extensions: &http::response::Extensions,
    ) -> Result<()> {
        // This is an internal API misuse if this is triggered (calling
        // `handle_room_response` before this function), so panic is fine.
        assert!(self.response.is_none());

        self.to_device_events =
            self.client.base_client().process_sliding_sync_e2ee(extensions).await?;

        // Some new keys might have been received, so trigger a backup if needed.
        self.client.encryption().backups().maybe_trigger_backup();

        Ok(())
    }

    pub async fn handle_room_response(&mut self, response: &http::Response) -> Result<()> {
        self.response = Some(
            self.client
                .base_client()
                .process_sliding_sync(
                    response,
                    &SlidingSyncPreviousEventsProvider(self.rooms),
                    self.client.is_simplified_sliding_sync_enabled(),
                )
                .await?,
        );
        self.post_process().await
    }

    async fn post_process(&mut self) -> Result<()> {
        // This is an internal API misuse if this is triggered (calling
        // `handle_room_response` after this function), so panic is fine.
        let response = self.response.as_ref().unwrap();

        update_in_memory_caches(&self.client, response).await?;

        Ok(())
    }

    pub async fn process_and_take_response(mut self) -> Result<SyncResponse> {
        let mut response = self.response.take().unwrap_or_default();

        response.to_device.extend(self.to_device_events);

        self.client.call_sync_response_handlers(&response).await?;

        Ok(response)
    }
}

/// Update the caches for the rooms that received updates.
///
/// This will only fill the in-memory caches, not save the info on disk.
async fn update_in_memory_caches(client: &Client, response: &SyncResponse) -> Result<()> {
    for room_id in response.rooms.join.keys() {
        let Some(room) = client.get_room(room_id) else {
            error!(room_id = ?room_id, "Cannot post process a room in sliding sync because it is missing");
            continue;
        };

        room.user_defined_notification_mode().await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use matrix_sdk_base::notification_settings::RoomNotificationMode;
    use matrix_sdk_test::async_test;
    use ruma::{assign, room_id, serde::Raw};
    use serde_json::json;

    use crate::{
        error::Result, sliding_sync::http, test_utils::logged_in_client_with_server,
        SlidingSyncList, SlidingSyncMode,
    };

    #[async_test]
    async fn test_cache_user_defined_notification_mode() -> Result<()> {
        let (client, _server) = logged_in_client_with_server().await;
        let room_id = room_id!("!r0:matrix.org");

        let sliding_sync = client
            .sliding_sync("test")?
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10)),
            )
            .build()
            .await?;

        // Mock a sync response.
        // A `m.push_rules` with `room` is cached during the sync.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                rooms: BTreeMap::from([(
                    room_id.to_owned(),
                    http::response::Room::default(),
                )]),
                extensions: assign!(http::response::Extensions::default(), {
                    account_data: assign!(http::response::AccountData::default(), {
                        global: vec![
                            Raw::from_json_string(
                                json!({
                                    "type": "m.push_rules",
                                    "content": {
                                        "global": {
                                            "room": [
                                                {
                                                    "actions": ["notify"],
                                                    "rule_id": room_id,
                                                    "default": false,
                                                    "enabled": true,
                                                },
                                            ],
                                        },
                                    },
                                })
                                .to_string(),
                            ).unwrap()
                        ]
                    })
                })
            });

            let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
            sliding_sync.handle_response(server_response.clone(), &mut pos_guard).await?;
        }

        // The room must exist, since it's been synced.
        let room = client.get_room(room_id).unwrap();

        // The room has a cached user-defined notification mode.
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::AllMessages),
        );

        // Mock a sync response.
        // A `m.push_rules` with `room` is cached during the sync.
        // It overwrites the previous cache.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                rooms: BTreeMap::from([(
                    room_id.to_owned(),
                    http::response::Room::default(),
                )]),
                extensions: assign!(http::response::Extensions::default(), {
                    account_data: assign!(http::response::AccountData::default(), {
                        global: vec![
                            Raw::from_json_string(
                                json!({
                                    "type": "m.push_rules",
                                    "content": {
                                        "global": {
                                            "room": [
                                                {
                                                    "actions": [],
                                                    "rule_id": room_id,
                                                    "default": false,
                                                    "enabled": true,
                                                },
                                            ],
                                        },
                                    },
                                })
                                .to_string(),
                            ).unwrap()
                        ]
                    })
                })
            });

            let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
            sliding_sync.handle_response(server_response.clone(), &mut pos_guard).await?;
        }

        // The room has an updated cached user-defined notification mode.
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::MentionsAndKeywordsOnly),
        );

        Ok(())
    }
}
