use matrix_sdk_base::{StateStoreDataKey, StateStoreDataValue};
use ruma::{
    api::client::discovery::{
        get_capabilities,
        get_capabilities::v3::{Capabilities, ProfileFieldsCapability},
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
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields
            && profile_fields.enabled
        {
            let allowed = profile_fields.allowed.unwrap_or_default();
            let disallowed = profile_fields.disallowed.unwrap_or_default();
            return Ok(allowed.contains(&ProfileFieldName::DisplayName)
                || !disallowed.contains(&ProfileFieldName::DisplayName));
        }
        #[allow(deprecated)]
        Ok(capabilities.set_displayname.enabled)
    }

    /// Returns whether the user can change their avatar or not.
    ///
    /// This will first check the `m.profile_fields` capability and use it if
    /// present, or fall back to `m.set_avatar_url` otherwise.
    ///
    /// Spec: <https://spec.matrix.org/latest/client-server-api/#mset_avatar_url-capability>
    pub async fn can_change_avatar(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields
            && profile_fields.enabled
        {
            let allowed = profile_fields.allowed.unwrap_or_default();
            let disallowed = profile_fields.disallowed.unwrap_or_default();
            return Ok(allowed.contains(&ProfileFieldName::AvatarUrl)
                || !disallowed.contains(&ProfileFieldName::AvatarUrl));
        }
        #[allow(deprecated)]
        Ok(capabilities.set_avatar_url.enabled)
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
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields {
            return Ok(profile_fields);
        }
        Ok(ProfileFieldsCapability::new(false))
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
}

#[cfg(all(not(target_family = "wasm"), test))]
mod tests {
    use matrix_sdk_test::async_test;

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
}
