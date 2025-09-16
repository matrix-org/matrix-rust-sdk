// Copyright 2025 Kévin Commaille
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

//! Types and functions related to the account management URL.
//!
//! This is a Matrix extension introduced in [MSC4191](https://github.com/matrix-org/matrix-spec-proposals/pull/4191).

use ruma::{
    OwnedDeviceId,
    api::client::discovery::get_authorization_server_metadata::v1::AccountManagementAction,
};
use url::Url;

/// An account management action that a user can take, including a device ID for
/// the actions that support it.
///
/// The actions are defined in [MSC4191].
///
/// [MSC4191]: https://github.com/matrix-org/matrix-spec-proposals/pull/4191
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountManagementActionFull {
    /// `org.matrix.profile`
    ///
    /// The user wishes to view their profile (name, avatar, contact details).
    Profile,

    /// `org.matrix.sessions_list`
    ///
    /// The user wishes to view a list of their sessions.
    SessionsList,

    /// `org.matrix.session_view`
    ///
    /// The user wishes to view the details of a specific session.
    SessionView {
        /// The ID of the session to view the details of.
        device_id: OwnedDeviceId,
    },

    /// `org.matrix.session_end`
    ///
    /// The user wishes to end/log out of a specific session.
    SessionEnd {
        /// The ID of the session to end.
        device_id: OwnedDeviceId,
    },

    /// `org.matrix.account_deactivate`
    ///
    /// The user wishes to deactivate their account.
    AccountDeactivate,

    /// `org.matrix.cross_signing_reset`
    ///
    /// The user wishes to reset their cross-signing keys.
    CrossSigningReset,
}

impl AccountManagementActionFull {
    /// Get the [`AccountManagementAction`] matching this
    /// [`AccountManagementActionFull`].
    pub fn action_type(&self) -> AccountManagementAction {
        match self {
            Self::Profile => AccountManagementAction::Profile,
            Self::SessionsList => AccountManagementAction::SessionsList,
            Self::SessionView { .. } => AccountManagementAction::SessionView,
            Self::SessionEnd { .. } => AccountManagementAction::SessionEnd,
            Self::AccountDeactivate => AccountManagementAction::AccountDeactivate,
            Self::CrossSigningReset => AccountManagementAction::CrossSigningReset,
        }
    }

    /// Append this action to the query of the given URL.
    fn append_to_url(&self, url: &mut Url) {
        let mut query_pairs = url.query_pairs_mut();
        query_pairs.append_pair("action", self.action_type().as_str());

        match self {
            Self::SessionView { device_id } | Self::SessionEnd { device_id } => {
                query_pairs.append_pair("device_id", device_id.as_str());
            }
            _ => {}
        }
    }
}

/// Builder for the URL for accessing the account management capabilities, as
/// defined in [MSC4191].
///
/// This type can be instantiated with [`OAuth::account_management_url()`] and
/// [`OAuth::fetch_account_management_url()`].
///
/// [`AccountManagementUrlBuilder::build()`] returns a URL to be opened in a web
/// browser where the end-user will be able to access the account management
/// capabilities of the issuer.
///
/// # Example
///
/// ```no_run
/// use matrix_sdk::authentication::oauth::AccountManagementActionFull;
/// # _ = async {
/// # let client: matrix_sdk::Client = unimplemented!();
/// let oauth = client.oauth();
///
/// // Get the account management URL from the server metadata.
/// let Some(url_builder) = oauth.account_management_url().await? else {
///     println!("The server doesn't advertise an account management URL");
///     return Ok(());
/// };
///
/// // The user wants to see the list of sessions.
/// let url =
///     url_builder.action(AccountManagementActionFull::SessionsList).build();
///
/// println!("See your sessions at: {url}");
/// # anyhow::Ok(()) };
/// ```
///
/// [MSC4191]: https://github.com/matrix-org/matrix-spec-proposals/pull/4191
/// [`OAuth::account_management_url()`]: super::OAuth::account_management_url
/// [`OAuth::fetch_account_management_url()`]: super::OAuth::fetch_account_management_url
#[derive(Debug, Clone)]
pub struct AccountManagementUrlBuilder {
    account_management_uri: Url,
    action: Option<AccountManagementActionFull>,
}

impl AccountManagementUrlBuilder {
    /// Construct an [`AccountManagementUrlBuilder`] for the given URL.
    pub(super) fn new(account_management_uri: Url) -> Self {
        Self { account_management_uri, action: None }
    }

    /// Set the action that the user wishes to take.
    pub fn action(mut self, action: AccountManagementActionFull) -> Self {
        self.action = Some(action);
        self
    }

    /// Build the URL to present to the end user.
    pub fn build(self) -> Url {
        // Add our parameters to the query, because the URL might already have one.
        let mut account_management_uri = self.account_management_uri;

        if let Some(action) = &self.action {
            action.append_to_url(&mut account_management_uri);
        }

        account_management_uri
    }
}

#[cfg(test)]
mod tests {
    use ruma::owned_device_id;
    use url::Url;

    use super::{AccountManagementActionFull, AccountManagementUrlBuilder};

    #[test]
    fn test_build_account_management_url_actions() {
        let base_url = Url::parse("https://example.org").unwrap();
        let device_id = owned_device_id!("ABCDEFG");

        let url = AccountManagementUrlBuilder::new(base_url.clone()).build();
        assert_eq!(url, base_url);

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::Profile)
            .build();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.profile");

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionsList)
            .build();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.sessions_list");

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionView { device_id: device_id.clone() })
            .build();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_view&device_id=ABCDEFG"
        );

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionEnd { device_id })
            .build();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_end&device_id=ABCDEFG"
        );

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::AccountDeactivate)
            .build();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.account_deactivate");

        let url = AccountManagementUrlBuilder::new(base_url)
            .action(AccountManagementActionFull::CrossSigningReset)
            .build();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.cross_signing_reset");
    }

    #[test]
    fn test_build_account_management_url_with_query() {
        let base_url = Url::parse("https://example.org/?sid=123456").unwrap();

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::Profile)
            .build();
        assert_eq!(url.as_str(), "https://example.org/?sid=123456&action=org.matrix.profile");

        let url = AccountManagementUrlBuilder::new(base_url)
            .action(AccountManagementActionFull::SessionView {
                device_id: owned_device_id!("ABCDEFG"),
            })
            .build();
        assert_eq!(
            url.as_str(),
            "https://example.org/?sid=123456&action=org.matrix.session_view&device_id=ABCDEFG"
        );
    }
}
