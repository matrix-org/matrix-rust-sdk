use std::collections::BTreeSet;

use matrix_sdk_base::{sync::SyncResponse, RequestedRequiredStates};
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use ruma::api::client::{discovery::get_supported_versions, sync::sync_events::v5 as http};
use tracing::error;

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{Client, Result};

/// A sliding sync version.
#[derive(Clone, Debug)]
pub enum Version {
    /// No version. Useful to represent that sliding sync is disabled for
    /// example, and that the version is unknown.
    None,

    /// Use the version of the sliding sync implementation inside Synapse, i.e.
    /// MSC4186.
    Native,
}

impl Version {
    #[cfg(test)]
    pub(crate) fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }
}

/// An error when building a version.
#[derive(thiserror::Error, Debug)]
pub enum VersionBuilderError {
    /// The `.well-known` response is not set.
    #[error("`.well-known` is not set")]
    WellKnownNotSet,

    /// The `/versions` response is not set.
    #[error("The `/versions` response is not set")]
    MissingVersionsResponse,

    /// `/versions` does not contain `org.matrix.simplified_msc3575` in its
    /// `unstable_features`, or it's not set to true.
    #[error("`/versions` does not contain `org.matrix.simplified_msc3575` in its `unstable_features`, or it's not set to true.")]
    NativeVersionIsUnset,
}

/// A builder for [`Version`].
#[derive(Clone, Debug)]
pub enum VersionBuilder {
    /// Build a [`Version::None`].
    None,

    /// Build a [`Version::Native`].
    Native,

    /// Build a [`Version::Native`] by auto-discovering it.
    ///
    /// It is available if the server enables it via `/versions`.
    DiscoverNative,
}

impl VersionBuilder {
    pub(crate) fn needs_get_supported_versions(&self) -> bool {
        matches!(self, Self::DiscoverNative)
    }

    /// Build a [`Version`].
    ///
    /// It can fail if auto-discovering fails, e.g. if `/versions` do contain
    /// invalid data.
    pub fn build(
        self,
        versions: Option<&get_supported_versions::Response>,
    ) -> Result<Version, VersionBuilderError> {
        Ok(match self {
            Self::None => Version::None,

            Self::Native => Version::Native,

            Self::DiscoverNative => {
                let Some(versions) = versions else {
                    return Err(VersionBuilderError::MissingVersionsResponse);
                };

                match versions.unstable_features.get("org.matrix.simplified_msc3575") {
                    Some(value) if *value => Version::Native,
                    _ => return Err(VersionBuilderError::NativeVersionIsUnset),
                }
            }
        })
    }
}

impl Client {
    /// Find all sliding sync versions that are available.
    ///
    /// Be careful: This method may hit the store and will send new requests for
    /// each call. It can be costly to call it repeatedly.
    ///
    /// If `.well-known` or `/versions` is unreachable, it will simply move
    /// potential sliding sync versions aside. No error will be reported.
    pub async fn available_sliding_sync_versions(&self) -> Vec<Version> {
        let supported_versions = self.unstable_features().await.ok().map(|unstable_features| {
            let mut response = get_supported_versions::Response::new(vec![]);
            response.unstable_features = unstable_features;

            response
        });

        [VersionBuilder::DiscoverNative]
            .into_iter()
            .filter_map(|version_builder| version_builder.build(supported_versions.as_ref()).ok())
            .collect()
    }

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
        requested_required_states: &RequestedRequiredStates,
    ) -> Result<SyncResponse> {
        let response =
            self.base_client().process_sliding_sync(response, requested_required_states).await?;

        tracing::debug!("done processing on base_client");
        self.call_sync_response_handlers(&response).await?;

        Ok(response)
    }
}

/// Small helper to handle a `SlidingSync` response's sub parts.
///
/// This will properly handle the encryption and the room response
/// independently, if needs be, making sure that both are properly processed by
/// event handlers.
#[must_use]
pub(crate) struct SlidingSyncResponseProcessor {
    client: Client,
    to_device_events: Vec<ProcessedToDeviceEvent>,
    response: Option<SyncResponse>,
}

impl SlidingSyncResponseProcessor {
    pub fn new(client: Client) -> Self {
        Self { client, to_device_events: Vec::new(), response: None }
    }

    #[cfg(feature = "e2e-encryption")]
    pub async fn handle_encryption(
        &mut self,
        extensions: &http::response::Extensions,
    ) -> Result<()> {
        // This is an internal API misuse if this is triggered (calling
        // `handle_room_response` before this function), so panic is fine.
        assert!(self.response.is_none());

        self.to_device_events = if let Some(to_device_events) = self
            .client
            .base_client()
            .process_sliding_sync_e2ee(extensions.to_device.as_ref(), &extensions.e2ee)
            .await?
        {
            // Some new keys might have been received, so trigger a backup if needed.
            self.client.encryption().backups().maybe_trigger_backup();

            to_device_events
        } else {
            Vec::new()
        };

        Ok(())
    }

    pub async fn handle_room_response(
        &mut self,
        response: &http::Response,
        requested_required_states: &RequestedRequiredStates,
    ) -> Result<()> {
        let mut sync_response = self
            .client
            .base_client()
            .process_sliding_sync(response, requested_required_states)
            .await?;
        handle_receipts_extension(&self.client, response, &mut sync_response).await?;

        self.response = Some(sync_response);
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
    for room_id in response.rooms.joined.keys() {
        let Some(room) = client.get_room(room_id) else {
            error!(?room_id, "Cannot post process a room in sliding sync because it is missing");
            continue;
        };

        room.user_defined_notification_mode().await;
    }

    Ok(())
}

/// Update the receipts extension and compute the read receipt accordingly.
async fn handle_receipts_extension(
    client: &Client,
    response: &http::Response,
    sync_response: &mut SyncResponse,
) -> Result<()> {
    // We need to compute read receipts for each joined room that has received an
    // update, or from each room that has received a receipt ephemeral event.
    let room_ids = BTreeSet::from_iter(
        sync_response
            .rooms
            .joined
            .keys()
            .cloned()
            .chain(response.extensions.receipts.rooms.keys().cloned()),
    );

    for room_id in room_ids {
        let Ok((room_event_cache, _drop_handle)) = client.event_cache().for_room(&room_id).await
        else {
            tracing::info!(
                ?room_id,
                "Failed to fetch the `RoomEventCache` when computing unread counts"
            );

            continue;
        };

        let previous_events = room_event_cache.events().await;

        client
            .base_client()
            .process_sliding_sync_receipts_extension_for_room(
                &room_id,
                response,
                sync_response,
                previous_events,
            )
            .await?;
    }
    Ok(())
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        notification_settings::RoomNotificationMode, RequestedRequiredStates,
        RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons,
    };
    use matrix_sdk_test::async_test;
    use ruma::{assign, events::AnySyncTimelineEvent, room_id, serde::Raw};
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };

    use super::{get_supported_versions, Version, VersionBuilder};
    use crate::{
        error::Result,
        sliding_sync::{client::SlidingSyncResponseProcessor, http, VersionBuilderError},
        test_utils::{logged_in_client, logged_in_client_with_server},
        SlidingSyncList, SlidingSyncMode,
    };

    #[test]
    fn test_version_builder_none() {
        assert_matches!(VersionBuilder::None.build(None), Ok(Version::None));
    }

    #[test]
    fn test_version_builder_native() {
        assert_matches!(VersionBuilder::Native.build(None), Ok(Version::Native));
    }

    #[test]
    fn test_version_builder_discover_native() {
        let mut response = get_supported_versions::Response::new(vec![]);
        response.unstable_features = [("org.matrix.simplified_msc3575".to_owned(), true)].into();

        assert_matches!(VersionBuilder::DiscoverNative.build(Some(&response)), Ok(Version::Native));
    }

    #[test]
    fn test_version_builder_discover_native_no_supported_versions() {
        assert_matches!(
            VersionBuilder::DiscoverNative.build(None),
            Err(VersionBuilderError::MissingVersionsResponse)
        );
    }

    #[test]
    fn test_version_builder_discover_native_unstable_features_is_disabled() {
        let mut response = get_supported_versions::Response::new(vec![]);
        response.unstable_features = [("org.matrix.simplified_msc3575".to_owned(), false)].into();

        assert_matches!(
            VersionBuilder::DiscoverNative.build(Some(&response)),
            Err(VersionBuilderError::NativeVersionIsUnset)
        );
    }

    #[async_test]
    async fn test_available_sliding_sync_versions_none() {
        let (client, _server) = logged_in_client_with_server().await;
        let available_versions = client.available_sliding_sync_versions().await;

        // `.well-known` and `/versions` aren't available. It's impossible to find any
        // versions.
        assert!(available_versions.is_empty());
    }

    #[async_test]
    async fn test_available_sliding_sync_versions_native() {
        let (client, server) = logged_in_client_with_server().await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "versions": [],
                "unstable_features": {
                    "org.matrix.simplified_msc3575": true,
                },
            })))
            .mount(&server)
            .await;

        let available_versions = client.available_sliding_sync_versions().await;

        // `/versions` is available.
        assert_eq!(available_versions.len(), 1);
        assert_matches!(available_versions[0], Version::Native);
    }

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
            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut pos_guard,
                    RequestedRequiredStates::default(),
                )
                .await?;
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
            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut pos_guard,
                    RequestedRequiredStates::default(),
                )
                .await?;
        }

        // The room has an updated cached user-defined notification mode.
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::MentionsAndKeywordsOnly),
        );

        Ok(())
    }

    #[async_test]
    async fn test_read_receipt_can_trigger_a_notable_update_reason() {
        use ruma::api::client::sync::sync_events::v5 as http;

        // Given a logged-in client
        let client = logged_in_client(None).await;
        client.event_cache().subscribe().unwrap();

        let mut room_info_notable_update_stream = client.room_info_notable_update_receiver();

        // When I send sliding sync response containing a new room.
        let room_id = room_id!("!r:e.uk");
        let room = http::response::Room::new();
        let mut response = http::Response::new("5".to_owned());
        response.rooms.insert(room_id.to_owned(), room);

        let mut processor = SlidingSyncResponseProcessor::new(client.clone());
        processor
            .handle_room_response(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        processor.process_and_take_response().await.expect("Failed to finish processing sync");

        // Then room info notable updates are received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(!received_reasons.contains(RoomInfoNotableUpdateReasons::READ_RECEIPT), "{received_reasons:?}");
            }
        );
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I send sliding sync response containing a couple of events with no read
        // receipt.
        let room_id = room_id!("!r:e.uk");
        let events = vec![
            make_raw_event("m.room.message", "$3"),
            make_raw_event("m.room.message", "$4"),
            make_raw_event("m.read", "$5"),
        ];
        let room = assign!(http::response::Room::new(), {
            timeline: events,
        });
        let mut response = http::Response::new("5".to_owned());
        response.rooms.insert(room_id.to_owned(), room);

        let mut processor = SlidingSyncResponseProcessor::new(client.clone());
        processor
            .handle_room_response(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        processor.process_and_take_response().await.expect("Failed to finish processing sync");

        // Then room info notable updates are received.
        //
        // `NONE` because the regular sync process ends up to updating nothing.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE), "{received_reasons:?}");
            }
        );
        // `READ_RECEIPT` because this is what we expect.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::READ_RECEIPT), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());
    }

    fn make_raw_event(event_type: &str, id: &str) -> Raw<AnySyncTimelineEvent> {
        Raw::from_json_string(
            json!({
                "type": event_type,
                "event_id": id,
                "content": { "msgtype": "m.text", "body": "my msg" },
                "sender": "@u:h.uk",
                "origin_server_ts": 12344445,
            })
            .to_string(),
        )
        .unwrap()
    }
}
