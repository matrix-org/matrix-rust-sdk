use itertools::Itertools;
use matrix_sdk_base::{StateStoreDataKey, StateStoreDataValue};
use ruma::api::client::{
    discovery::{
        get_capabilities,
        get_capabilities::v3::{Capabilities, ProfileFieldsCapability},
    },
    profile::ProfileFieldName,
};
use tracing::warn;

use crate::{Client, HttpResult};

#[derive(Debug, Clone)]
pub struct HomeserverCapabilities {
    client: Client,
}

impl HomeserverCapabilities {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn refresh(&self) -> crate::Result<()> {
        self.get_and_cache_remote_capabilities().await?;
        Ok(())
    }

    pub async fn can_change_password(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.change_password.enabled)
    }

    pub async fn can_change_displayname(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields {
            if profile_fields.enabled {
                let allowed = profile_fields.allowed.unwrap_or_default();
                let disallowed = profile_fields.disallowed.unwrap_or_default();
                return Ok(allowed.contains(&ProfileFieldName::DisplayName)
                    || !disallowed.contains(&ProfileFieldName::DisplayName));
            }
        }
        #[allow(deprecated)]
        Ok(capabilities.set_displayname.enabled)
    }

    pub async fn can_change_avatar(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields {
            if profile_fields.enabled {
                let allowed = profile_fields.allowed.unwrap_or_default();
                let disallowed = profile_fields.disallowed.unwrap_or_default();
                return Ok(allowed.contains(&ProfileFieldName::AvatarUrl)
                    || !disallowed.contains(&ProfileFieldName::AvatarUrl));
            }
        }
        #[allow(deprecated)]
        Ok(capabilities.set_avatar_url.enabled)
    }

    pub async fn can_change_thirdparty_ids(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.thirdparty_id_changes.enabled)
    }

    pub async fn can_get_login_token(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.get_login_token.enabled)
    }

    pub async fn extended_profile_fields(&self) -> crate::Result<ProfileFieldsCapability> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        if let Some(profile_fields) = capabilities.profile_fields {
            return Ok(profile_fields);
        }
        Ok(ProfileFieldsCapability::new(false))
    }

    pub async fn forgets_room_when_leaving(&self) -> crate::Result<bool> {
        let capabilities = self.load_or_fetch_homeserver_capabilities().await?;
        Ok(capabilities.forget_forced_upon_leave.enabled)
    }

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

    /// Get the capabilities of the homeserver.
    ///
    /// This method should be used to check what features are supported by the
    /// homeserver.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let capabilities = client.get_capabilities().await?;
    ///
    /// if capabilities.change_password.enabled {
    ///     // Change password
    /// }
    /// # anyhow::Ok(()) };
    /// ```
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
            warn!("error when caching homeserver capabilites: {err}");
        }

        Ok(res.capabilities)
    }
}
