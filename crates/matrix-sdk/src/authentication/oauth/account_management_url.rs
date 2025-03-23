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

use ruma::OwnedDeviceId;
use serde::Serialize;
use url::Url;

/// An account management action that a user can take, including a device ID for
/// the actions that support it.
///
/// The actions are defined in [MSC4191].
///
/// [MSC4191]: https://github.com/matrix-org/matrix-spec-proposals/pull/4191
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action")]
#[non_exhaustive]
pub enum AccountManagementActionFull {
    /// `org.matrix.profile`
    ///
    /// The user wishes to view their profile (name, avatar, contact details).
    #[serde(rename = "org.matrix.profile")]
    Profile,

    /// `org.matrix.sessions_list`
    ///
    /// The user wishes to view a list of their sessions.
    #[serde(rename = "org.matrix.sessions_list")]
    SessionsList,

    /// `org.matrix.session_view`
    ///
    /// The user wishes to view the details of a specific session.
    #[serde(rename = "org.matrix.session_view")]
    SessionView {
        /// The ID of the session to view the details of.
        device_id: OwnedDeviceId,
    },

    /// `org.matrix.session_end`
    ///
    /// The user wishes to end/log out of a specific session.
    #[serde(rename = "org.matrix.session_end")]
    SessionEnd {
        /// The ID of the session to end.
        device_id: OwnedDeviceId,
    },

    /// `org.matrix.account_deactivate`
    ///
    /// The user wishes to deactivate their account.
    #[serde(rename = "org.matrix.account_deactivate")]
    AccountDeactivate,

    /// `org.matrix.cross_signing_reset`
    ///
    /// The user wishes to reset their cross-signing keys.
    #[serde(rename = "org.matrix.cross_signing_reset")]
    CrossSigningReset,
}

/// Builder for the URL for accessing the account management capabilities, as
/// defined in [MSC4191].
///
/// This type can be instantiated with [`OAuth::account_management_url()`] and
/// [`OAuth::fetch_account_management_url()`].
///
/// [`AccountManagementUrlBuilder::build()`] and
/// [`AccountManagementUrlBuilder::build_or_ignore_action()`] return a URL to be
/// opened in a web browser where the end-user will be able to access the
/// account management capabilities of the issuer.
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

    /// Get the serialized action, if any.
    fn serialized_action(&self) -> Result<Option<String>, serde_html_form::ser::Error> {
        self.action.as_ref().map(serde_html_form::to_string).transpose()
    }

    /// Build the URL with the given serialized action.
    fn build_inner(self, serialized_action: Option<String>) -> Url {
        let Some(serialized_action) = serialized_action else {
            return self.account_management_uri;
        };

        // Add our parameters to the query, because the URL might already have one.
        let mut account_management_uri = self.account_management_uri;
        let mut full_query =
            account_management_uri.query().map(ToOwned::to_owned).unwrap_or_default();

        if !full_query.is_empty() {
            full_query.push('&');
        }
        full_query.push_str(&serialized_action);

        account_management_uri.set_query(Some(&full_query));

        account_management_uri
    }

    /// Build the URL to present to the end user.
    ///
    /// # Errors
    ///
    /// Returns an error if serializing the action fails.
    pub fn build(self) -> Result<Url, serde_html_form::ser::Error> {
        let serialized_action = self.serialized_action()?;
        Ok(self.build_inner(serialized_action))
    }

    /// Build the URL to present to the end user and ignore errors while
    /// serializing the action.
    ///
    /// Returns the URL without an action if the action fails to serialize.
    pub fn build_or_ignore_action(self) -> Url {
        let serialized_action = self.serialized_action().ok().flatten();
        self.build_inner(serialized_action)
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

        let url = AccountManagementUrlBuilder::new(base_url.clone()).build().unwrap();
        assert_eq!(url, base_url);

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::Profile)
            .build()
            .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.profile");

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionsList)
            .build()
            .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.sessions_list");

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionView { device_id: device_id.clone() })
            .build()
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_view&device_id=ABCDEFG"
        );

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::SessionEnd { device_id })
            .build()
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_end&device_id=ABCDEFG"
        );

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::AccountDeactivate)
            .build()
            .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.account_deactivate");

        let url = AccountManagementUrlBuilder::new(base_url)
            .action(AccountManagementActionFull::CrossSigningReset)
            .build()
            .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.cross_signing_reset");
    }

    #[test]
    fn test_build_account_management_url_with_query() {
        let base_url = Url::parse("https://example.org/?sid=123456").unwrap();

        let url = AccountManagementUrlBuilder::new(base_url.clone())
            .action(AccountManagementActionFull::Profile)
            .build()
            .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?sid=123456&action=org.matrix.profile");

        let url = AccountManagementUrlBuilder::new(base_url)
            .action(AccountManagementActionFull::SessionView {
                device_id: owned_device_id!("ABCDEFG"),
            })
            .build()
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?sid=123456&action=org.matrix.session_view&device_id=ABCDEFG"
        );
    }

    #[test]
    fn test_build_account_management_url_or_ignore_action() {
        let base_url = Url::parse("https://example.org").unwrap();

        let url = AccountManagementUrlBuilder::new(base_url.clone()).build_or_ignore_action();
        assert_eq!(url, base_url);

        let url = AccountManagementUrlBuilder::new(base_url)
            .action(AccountManagementActionFull::Profile)
            .build_or_ignore_action();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.profile");
    }
}
