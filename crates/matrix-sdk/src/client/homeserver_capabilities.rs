use matrix_sdk_base::{StateStoreDataKey, StateStoreDataValue};
use ruma::{
    api::{
        Metadata,
        client::{
            discovery::get_capabilities::{
                self,
                v3::{
                    AccountModerationCapability, Capabilities, ProfileFieldsCapability,
                    RoomVersionsCapability,
                },
            },
            profile::delete_profile_field,
        },
    },
    profile::ProfileFieldName,
};
use tracing::warn;

use crate::{Client, HttpResult};

/// Helper to check what [`Capabilities`] are supported by the homeserver.
///
/// Spec: <https://spec.matrix.org/latest/client-server-api/#capabilities-negotiation>
#[derive(Debug, Clone)]
pub struct HomeserverCapabilities {
    client: Client,
}

impl HomeserverCapabilities {
    /// Creates a new [`HomeserverCapabilities`] instance.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Forces a refresh of the cached value using the `/capabilities` endpoint.
    pub async fn refresh(&self) -> crate::Result<()> {
        self.get_and_cache_remote_capabilities().await?;
        Ok(())
    }

    /// Returns whether the user can change their password or not.
    pub async fn can_change_password(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.change_password.enabled)
    }

    /// Returns whether the user can change their display name or not.
    ///
    /// This will first check the `m.profile_fields` capability and use it if
    /// present, or fall back to `m.set_displayname` otherwise.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mset_displayname-capability>
    pub async fn can_change_displayname(&self) -> crate::Result<bool> {
        let capabilities = self.profile_capabilities().await?;

        if let Some(profile_fields) = capabilities.profile_fields {
            Ok(profile_fields.can_set_field(&ProfileFieldName::DisplayName))
        } else {
            Ok(capabilities.set_displayname)
        }
    }

    /// Returns whether the user can change their avatar or not.
    ///
    /// This will first check the `m.profile_fields` capability and use it if
    /// present, or fall back to `m.set_avatar_url` otherwise.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mset_avatar_url-capability>
    pub async fn can_change_avatar(&self) -> crate::Result<bool> {
        let capabilities = self.profile_capabilities().await?;

        if let Some(profile_fields) = capabilities.profile_fields {
            Ok(profile_fields.can_set_field(&ProfileFieldName::AvatarUrl))
        } else {
            Ok(capabilities.set_avatar_url)
        }
    }

    /// Returns whether the user can add, remove, or change 3PID associations on
    /// their account.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#m3pid_changes-capability>
    pub async fn can_change_thirdparty_ids(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.thirdparty_id_changes.enabled)
    }

    /// Returns whether the user is able to use `POST /login/get_token` to
    /// generate single-use, time-limited tokens to log unauthenticated
    /// clients into their account.
    ///
    /// When not listed, clients SHOULD assume the user is unable to generate
    /// tokens.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mget_login_token-capability>
    pub async fn can_get_login_token(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.get_login_token.enabled)
    }

    /// Returns which profile fields the user is able to change.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mprofile_fields-capability>
    pub async fn extended_profile_fields(&self) -> crate::Result<ProfileFieldsCapability> {
        Ok(self
            .profile_capabilities()
            .await?
            .profile_fields
            .unwrap_or_else(|| ProfileFieldsCapability::new(false)))
    }

    /// Returns the room versions supported by the server.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mroom_versions-capability>
    pub async fn room_versions(&self) -> crate::Result<RoomVersionsCapability> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.room_versions)
    }

    /// Returns whether the user can perform account moderation actions.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#get_matrixclientv3capabilities_response-200_accountmoderationcapability>
    pub async fn account_moderation(&self) -> crate::Result<AccountModerationCapability> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.account_moderation)
    }

    /// Returns whether or not the server automatically forgets rooms which the
    /// user has left.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mforget_forced_upon_leave-capability>
    pub async fn forgets_room_when_leaving(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.forget_forced_upon_leave.enabled)
    }

    /// Gets the supported [`Capabilities`] either from the local cache or from
    /// the homeserver using the `/capabilities` endpoint if the data is not
    /// cached.
    ///
    /// To ensure you get updated values, you should call [`Self::refresh`]
    /// instead.
    async fn load_or_fetch_homeserver_capabilities(&self) -> crate::Result<Capabilities> {
        match self.client.state_store().get_kv_data(StateStoreDataKey::HomeserverCapabilities).await
        {
            Ok(Some(stored)) => {
                if let Some(capabilities) = stored.into_homeserver_capabilities() {
                    return Ok(capabilities);
                }
            }
            Ok(None) => {
                // fallthrough: cache is empty
            }
            Err(err) => {
                warn!("error when loading cached homeserver capabilities: {err}");
                // fallthrough to network.
            }
        }

        Ok(self.get_and_cache_remote_capabilities().await?)
    }

    /// Gets and caches the capabilities of the homeserver.
    async fn get_and_cache_remote_capabilities(&self) -> HttpResult<Capabilities> {
        let res = self.client.send(get_capabilities::v3::Request::new()).await?;

        if let Err(err) = self
            .client
            .state_store()
            .set_kv_data(
                StateStoreDataKey::HomeserverCapabilities,
                StateStoreDataValue::HomeserverCapabilities(res.capabilities.clone()),
            )
            .await
        {
            warn!("error when caching homeserver capabilities: {err}");
        }

        Ok(res.capabilities)
    }

    /// Gets or computes the supported [`ProfileCapabilities`].
    async fn profile_capabilities(&self) -> crate::Result<ProfileCapabilities> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;

        let profile_fields = match capabilities.profile_fields {
            Some(profile_fields) => Some(profile_fields),
            None => {
                // According to the Matrix spec about the `m.profile_fields` capability:
                //
                // > When this capability is not listed, clients SHOULD assume the user is
                // > able to change profile fields without any restrictions, provided the
                // > homeserver advertises a specification version that includes the
                // > `m.profile_fields` capability in the `/versions` response.
                if self.homeserver_supports_extended_profile_fields().await? {
                    Some(ProfileFieldsCapability::new(true))
                } else {
                    None
                }
            }
        };

        #[allow(deprecated)]
        Ok(ProfileCapabilities {
            profile_fields,
            set_displayname: capabilities.set_displayname.enabled,
            set_avatar_url: capabilities.set_avatar_url.enabled,
        })
    }

    /// Whether the homeserver supports extended profile fields.
    ///
    ///
    /// [Matrix spec]: https://spec.matrix.org/latest/client-server-api/#mprofile_fields-capability
    async fn homeserver_supports_extended_profile_fields(&self) -> crate::Result<bool> {
        let supported_versions = self.client.supported_versions().await?;
        // If the homeserver supports the endpoint to delete profile fields, it supports
        // extended profile fields.
        Ok(delete_profile_field::v3::Request::PATH_BUILDER.is_supported(&supported_versions))
    }
}

/// All the capabilities to change a profile field.
struct ProfileCapabilities {
    /// The capability to change profile fields, advertised by the homeserver or
    /// computed.
    profile_fields: Option<ProfileFieldsCapability>,
    /// The capability to set the display name advertised by the homeserver.
    set_displayname: bool,
    /// The capability to set the avatar URL advertised by the homeserver.
    set_avatar_url: bool,
}

#[cfg(all(not(target_family = "wasm"), test))]
mod tests {
    use matrix_sdk_test::async_test;
    #[allow(deprecated)]
    use ruma::api::{
        MatrixVersion,
        client::discovery::get_capabilities::v3::{
            SetAvatarUrlCapability, SetDisplayNameCapability,
        },
    };

    use super::*;
    use crate::test_utils::mocks::MatrixMockServer;

    #[async_test]
    async fn test_refresh_always_updates_capabilities() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Set the expected capabilities to something we can check
        let mut expected_capabilities = Capabilities::default();
        expected_capabilities.change_password.enabled = true;
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(expected_capabilities)
            .mock_once()
            .mount()
            .await;

        // Refresh the capabilities
        let capabilities = client.homeserver_capabilities();
        capabilities.refresh().await.expect("refreshing capabilities failed");

        // Check the values we get are updated
        assert!(capabilities.can_change_password().await.expect("checking capabilities failed"));

        let mut expected_capabilities = Capabilities::default();
        expected_capabilities.change_password.enabled = false;
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(expected_capabilities)
            .mock_once()
            .mount()
            .await;

        // Check the values we get are not updated without a refresh, they're loaded
        // from the cache
        assert!(capabilities.can_change_password().await.expect("checking capabilities failed"));

        // Do another refresh to make sure we get the updated values
        capabilities.refresh().await.expect("refreshing capabilities failed");

        // Check the values we get are updated
        assert!(!capabilities.can_change_password().await.expect("checking capabilities failed"));
    }

    #[async_test]
    async fn test_get_functions_refresh_the_data_if_not_available_or_use_cache_if_available() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Set the expected capabilities to something we can check
        let mut expected_capabilities = Capabilities::default();
        let mut profile_fields = ProfileFieldsCapability::new(true);
        profile_fields.allowed = Some(vec![ProfileFieldName::DisplayName]);
        expected_capabilities.profile_fields = Some(profile_fields);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(expected_capabilities)
            // Ensure it's called just once
            .mock_once()
            .mount()
            .await;

        // Refresh the capabilities
        let capabilities = client.homeserver_capabilities();

        // Check the values we get are updated
        assert!(capabilities.can_change_displayname().await.expect("checking capabilities failed"));

        // Now revert the previous mock so we can check we're getting the cached value
        // instead of this one
        let mut expected_capabilities = Capabilities::default();
        let mut profile_fields = ProfileFieldsCapability::new(true);
        profile_fields.disallowed = Some(vec![ProfileFieldName::DisplayName]);
        expected_capabilities.profile_fields = Some(profile_fields);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(expected_capabilities)
            // Ensure it's not called, since we'd be using the cached values instead
            .never()
            .mount()
            .await;

        // Check the values we get are not updated without a refresh, they're loaded
        // from the cache
        assert!(capabilities.can_change_displayname().await.expect("checking capabilities failed"));
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_deprecated_profile_fields_capabilities() {
        let server = MatrixMockServer::new().await;

        // The user can only set the display name but not the avatar url or extended
        // profile fields.
        let mut capabilities = Capabilities::new();
        capabilities.profile_fields.take();
        capabilities.set_displayname = SetDisplayNameCapability::new(true);
        capabilities.set_avatar_url = SetAvatarUrlCapability::new(false);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(capabilities)
            // It should be called once by each client below.
            .expect(2)
            .mount()
            .await;

        // Client with Matrix 1.12 that did not support extended profile fields yet.
        // Because there is no `m.profile_fields` capability, we rely on the legacy
        // profile capabilities.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_12]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            !capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            !capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );

        // Client with Matrix 1.16 that added support for extended profile fields, the
        // deprecated profile capabilities are ignored.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_extended_profile_fields_capabilities_enabled() {
        let server = MatrixMockServer::new().await;

        // The user can set any profile field.
        // The legacy capabilities say differently, but they will be ignored.
        let mut capabilities = Capabilities::new();
        capabilities.profile_fields = Some(ProfileFieldsCapability::new(true));
        capabilities.set_displayname = SetDisplayNameCapability::new(true);
        capabilities.set_avatar_url = SetAvatarUrlCapability::new(false);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(capabilities)
            // It should be called once by each client below.
            .expect(2)
            .mount()
            .await;

        // Client with Matrix 1.12 that did not support extended profile fields yet.
        // However, because there is an `m.profile_fields` capability, we still rely on
        // it.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_12]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );

        // Client with Matrix 1.16 that added support for extended profile fields, only
        // the `m.profile_fields` capability is used too.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_extended_profile_fields_capabilities_disabled() {
        let server = MatrixMockServer::new().await;

        // The user cannot set any profile field.
        // The legacy capabilities say differently, but they will be ignored.
        let mut capabilities = Capabilities::new();
        capabilities.profile_fields = Some(ProfileFieldsCapability::new(false));
        capabilities.set_displayname = SetDisplayNameCapability::new(true);
        capabilities.set_avatar_url = SetAvatarUrlCapability::new(false);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(capabilities)
            // It should be called once by each client below.
            .expect(2)
            .mount()
            .await;

        // Client with Matrix 1.12 that did not support extended profile fields yet.
        // However, because there is an `m.profile_fields` capability, we still rely on
        // it.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_12]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            !capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            !capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            !capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );

        // Client with Matrix 1.16 that added support for extended profile fields, only
        // the `m.profile_fields` capability is used too.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            !capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            !capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            !capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_fine_grained_extended_profile_fields_capabilities() {
        let server = MatrixMockServer::new().await;

        // The user can only set the avatar URL.
        // The legacy capabilities say differently, but they will be ignored.
        let mut profile_fields = ProfileFieldsCapability::new(true);
        profile_fields.allowed = Some(vec![ProfileFieldName::AvatarUrl]);
        let mut capabilities = Capabilities::new();
        capabilities.profile_fields = Some(profile_fields);
        capabilities.set_displayname = SetDisplayNameCapability::new(true);
        capabilities.set_avatar_url = SetAvatarUrlCapability::new(false);
        server
            .mock_get_homeserver_capabilities()
            .ok_with_capabilities(capabilities)
            // It should be called once by each client below.
            .expect(2)
            .mount()
            .await;

        // Client with Matrix 1.12 that did not support extended profile fields yet.
        // However, because there is an `m.profile_fields` capability, we still rely on
        // it.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_12]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            !capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );

        // Client with Matrix 1.16 that added support for extended profile fields, only
        // the `m.profile_fields` capability is used too.
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
        let capabilities_api = client.homeserver_capabilities();
        assert!(
            !capabilities_api
                .can_change_displayname()
                .await
                .expect("checking displayname capability failed")
        );
        assert!(
            capabilities_api.can_change_avatar().await.expect("checking avatar capability failed")
        );
        assert!(
            capabilities_api
                .extended_profile_fields()
                .await
                .expect("checking profile fields capability failed")
                .enabled
        );
    }
}
