use std::collections::BTreeMap;

use as_variant::as_variant;
use imbl::Vector;
use matrix_sdk_base::{sliding_sync::http, sync::SyncResponse, PreviousEventsProvider};
use ruma::{
    api::{
        client::discovery::{discover_homeserver, get_supported_versions},
        MatrixVersion,
    },
    events::AnyToDeviceEvent,
    serde::Raw,
    OwnedRoomId,
};
use tracing::error;
use url::Url;

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{config::RequestConfig, Client, Result, SlidingSyncRoom};

/// A sliding sync version.
#[derive(Clone, Debug)]
pub enum Version {
    /// No version. Useful to represent that sliding sync is disabled for
    /// example, and that the version is unknown.
    None,

    /// Use the version of the sliding sync proxy, i.e. MSC3575.
    Proxy {
        /// URL to the proxy.
        url: Url,
    },

    /// Use the version of the sliding sync implementation inside Synapse, i.e.
    /// Simplified MSC3575.
    Native,
}

impl Version {
    pub(crate) fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    pub(crate) fn overriding_url(&self) -> Option<&Url> {
        as_variant!(self, Self::Proxy { url } => url)
    }
}

/// An error when building a version.
#[derive(thiserror::Error, Debug)]
pub enum VersionBuilderError {
    /// The `.well-known` response is not set.
    #[error("`.well-known` is not set")]
    WellKnownNotSet,

    /// `.well-known` does not contain a `sliding_sync_proxy` entry.
    #[error("`.well-known` does not contain a `sliding_sync_proxy` entry")]
    NoSlidingSyncInWellKnown,

    /// The `sliding_sync_proxy` URL in .well-known` is not valid ({0}).
    #[error("the `sliding_sync_proxy` URL in .well-known` is not valid ({0})")]
    UnparsableSlidingSyncUrl(url::ParseError),

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

    /// Build a [`Version::Proxy`].
    Proxy {
        /// Coerced URL to the proxy.
        url: Url,
    },

    /// Build a [`Version::Native`].
    Native,

    /// Build a [`Version::Proxy`] by auto-discovering it.
    ///
    /// It is available if the server enables it via `.well-known`.
    DiscoverProxy,

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
    /// It can fail if auto-discovering fails, e.g. if `.well-known`
    /// or `/versions` do contain invalid data.
    pub fn build(
        self,
        well_known: Option<&discover_homeserver::Response>,
        versions: Option<&get_supported_versions::Response>,
    ) -> Result<Version, VersionBuilderError> {
        Ok(match self {
            Self::None => Version::None,

            Self::Proxy { url } => Version::Proxy { url },

            Self::Native => Version::Native,

            Self::DiscoverProxy => {
                let Some(well_known) = well_known else {
                    return Err(VersionBuilderError::WellKnownNotSet);
                };

                let Some(info) = &well_known.sliding_sync_proxy else {
                    return Err(VersionBuilderError::NoSlidingSyncInWellKnown);
                };

                let url =
                    Url::parse(&info.url).map_err(VersionBuilderError::UnparsableSlidingSyncUrl)?;

                Version::Proxy { url }
            }

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
        let well_known = match self.server().map(ToString::to_string) {
            None => None,
            Some(server) => self
                .inner
                .http_client
                .send(
                    discover_homeserver::Request::new(),
                    Some(RequestConfig::short_retry()),
                    server,
                    None,
                    &[MatrixVersion::V1_0],
                    Default::default(),
                )
                .await
                .ok(),
        };
        let supported_versions = self.unstable_features().await.ok().map(|unstable_features| {
            let mut response = get_supported_versions::Response::new(vec![]);
            response.unstable_features = unstable_features;

            response
        });

        [VersionBuilder::DiscoverNative, VersionBuilder::DiscoverProxy]
            .into_iter()
            .filter_map(|version_builder| {
                version_builder.build(well_known.as_ref(), supported_versions.as_ref()).ok()
            })
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
    ) -> Result<SyncResponse> {
        let response = self
            .base_client()
            .process_sliding_sync(response, &(), self.sliding_sync_version().is_native())
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

    pub async fn handle_room_response(
        &mut self,
        response: &http::Response,
        from_simplified_sliding_sync: bool,
    ) -> Result<()> {
        self.response = Some(
            self.client
                .base_client()
                .process_sliding_sync(
                    response,
                    &SlidingSyncPreviousEventsProvider(self.rooms),
                    from_simplified_sliding_sync,
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

    use assert_matches::assert_matches;
    use matrix_sdk_base::notification_settings::RoomNotificationMode;
    use matrix_sdk_test::async_test;
    use ruma::{api::MatrixVersion, assign, room_id, serde::Raw, ServerName};
    use serde_json::json;
    use url::Url;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::{discover_homeserver, get_supported_versions, Version, VersionBuilder};
    use crate::{
        error::Result,
        sliding_sync::{http, VersionBuilderError},
        test_utils::logged_in_client_with_server,
        Client, SlidingSyncList, SlidingSyncMode,
    };

    #[test]
    fn test_version_builder_none() {
        assert_matches!(VersionBuilder::None.build(None, None), Ok(Version::None));
    }

    #[test]
    fn test_version_builder_proxy() {
        let expected_url = Url::parse("https://matrix.org:1234").unwrap();

        assert_matches!(
            VersionBuilder::Proxy { url: expected_url.clone() }.build(None, None),
            Ok(Version::Proxy { url }) => {
                assert_eq!(url, expected_url);
            }
        );
    }

    #[test]
    fn test_version_builder_native() {
        assert_matches!(VersionBuilder::Native.build(None, None), Ok(Version::Native));
    }

    #[test]
    fn test_version_builder_discover_proxy() {
        let expected_url = Url::parse("https://matrix.org:1234").unwrap();
        let mut response = discover_homeserver::Response::new(
            discover_homeserver::HomeserverInfo::new("matrix.org".to_owned()),
        );
        response.sliding_sync_proxy =
            Some(discover_homeserver::SlidingSyncProxyInfo::new(expected_url.to_string()));

        assert_matches!(
            VersionBuilder::DiscoverProxy.build(Some(&response), None),
            Ok(Version::Proxy { url }) => {
                assert_eq!(url, expected_url);
            }
        );
    }

    #[test]
    fn test_version_builder_discover_proxy_no_well_known() {
        assert_matches!(
            VersionBuilder::DiscoverProxy.build(None, None),
            Err(VersionBuilderError::WellKnownNotSet)
        );
    }

    #[test]
    fn test_version_builder_discover_proxy_no_sliding_sync_proxy_in_well_known() {
        let mut response = discover_homeserver::Response::new(
            discover_homeserver::HomeserverInfo::new("matrix.org".to_owned()),
        );
        response.sliding_sync_proxy = None; // already `None` but the test is clearer now.

        assert_matches!(
            VersionBuilder::DiscoverProxy.build(Some(&response), None),
            Err(VersionBuilderError::NoSlidingSyncInWellKnown)
        );
    }

    #[test]
    fn test_version_builder_discover_proxy_invalid_sliding_sync_proxy_in_well_known() {
        let mut response = discover_homeserver::Response::new(
            discover_homeserver::HomeserverInfo::new("matrix.org".to_owned()),
        );
        response.sliding_sync_proxy =
            Some(discover_homeserver::SlidingSyncProxyInfo::new("💥".to_owned()));

        assert_matches!(
            VersionBuilder::DiscoverProxy.build(Some(&response), None),
            Err(VersionBuilderError::UnparsableSlidingSyncUrl(err)) => {
                assert_eq!(err.to_string(), "relative URL without a base");
            }
        );
    }

    #[test]
    fn test_version_builder_discover_native() {
        let mut response = get_supported_versions::Response::new(vec![]);
        response.unstable_features = [("org.matrix.simplified_msc3575".to_owned(), true)].into();

        assert_matches!(
            VersionBuilder::DiscoverNative.build(None, Some(&response)),
            Ok(Version::Native)
        );
    }

    #[test]
    fn test_version_builder_discover_native_no_supported_versions() {
        assert_matches!(
            VersionBuilder::DiscoverNative.build(None, None),
            Err(VersionBuilderError::MissingVersionsResponse)
        );
    }

    #[test]
    fn test_version_builder_discover_native_unstable_features_is_disabled() {
        let mut response = get_supported_versions::Response::new(vec![]);
        response.unstable_features = [("org.matrix.simplified_msc3575".to_owned(), false)].into();

        assert_matches!(
            VersionBuilder::DiscoverNative.build(None, Some(&response)),
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
    async fn test_available_sliding_sync_versions_proxy() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "m.homeserver": {
                    "base_url": "https://matrix.org",
                },
                "org.matrix.msc3575.proxy": {
                    "url": "https://proxy.matrix.org",
                },
            })))
            .mount(&server)
            .await;

        let client = Client::builder()
            .insecure_server_name_no_tls(
                <&ServerName>::try_from(server.address().to_string().as_str()).unwrap(),
            )
            .server_versions([MatrixVersion::V1_0])
            .build()
            .await
            .unwrap();

        let available_versions = client.available_sliding_sync_versions().await;

        // `.well-known` is available.
        assert_eq!(available_versions.len(), 1);
        assert_matches!(
            &available_versions[0],
            Version::Proxy { url } => {
                assert_eq!(url, &Url::parse("https://proxy.matrix.org").unwrap());
            }
        );
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
