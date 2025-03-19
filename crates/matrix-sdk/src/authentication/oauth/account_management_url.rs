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

/// Build the URL for accessing the account management capabilities, as defined
/// in [MSC].
///
/// # Arguments
///
/// * `account_management_uri` - The URL to access the issuer's account
///   management capabilities.
///
/// * `action` - The action that the user wishes to take.
///
/// # Returns
///
/// A URL to be opened in a web browser where the end-user will be able to
/// access the account management capabilities of the issuer.
///
/// # Errors
///
/// Returns an error if serializing the action fails.
///
/// [MSC4191]: https://github.com/matrix-org/matrix-spec-proposals/pull/4191
pub(crate) fn build_account_management_url(
    mut account_management_uri: Url,
    action: AccountManagementActionFull,
) -> Result<Url, serde_html_form::ser::Error> {
    let extra_query = serde_html_form::to_string(action)?;

    // Add our parameters to the query, because the URL might already have one.
    let mut full_query = account_management_uri.query().map(ToOwned::to_owned).unwrap_or_default();

    if !full_query.is_empty() {
        full_query.push('&');
    }
    full_query.push_str(&extra_query);

    account_management_uri.set_query(Some(&full_query));

    Ok(account_management_uri)
}

#[cfg(test)]
mod tests {
    use ruma::owned_device_id;
    use url::Url;

    use super::{build_account_management_url, AccountManagementActionFull};

    #[test]
    fn test_build_account_management_url_actions() {
        let base_url = Url::parse("https://example.org").unwrap();
        let device_id = owned_device_id!("ABCDEFG");

        let url =
            build_account_management_url(base_url.clone(), AccountManagementActionFull::Profile)
                .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.profile");

        let url = build_account_management_url(
            base_url.clone(),
            AccountManagementActionFull::SessionsList,
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.sessions_list");

        let url = build_account_management_url(
            base_url.clone(),
            AccountManagementActionFull::SessionView { device_id: device_id.clone() },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_view&device_id=ABCDEFG"
        );

        let url = build_account_management_url(
            base_url.clone(),
            AccountManagementActionFull::SessionEnd { device_id },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?action=org.matrix.session_end&device_id=ABCDEFG"
        );

        let url = build_account_management_url(
            base_url.clone(),
            AccountManagementActionFull::AccountDeactivate,
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.account_deactivate");

        let url =
            build_account_management_url(base_url, AccountManagementActionFull::CrossSigningReset)
                .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?action=org.matrix.cross_signing_reset");
    }

    #[test]
    fn test_build_account_management_url_with_query() {
        let base_url = Url::parse("https://example.org/?sid=123456").unwrap();

        let url =
            build_account_management_url(base_url.clone(), AccountManagementActionFull::Profile)
                .unwrap();
        assert_eq!(url.as_str(), "https://example.org/?sid=123456&action=org.matrix.profile");

        let url = build_account_management_url(
            base_url,
            AccountManagementActionFull::SessionView { device_id: owned_device_id!("ABCDEFG") },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.org/?sid=123456&action=org.matrix.session_view&device_id=ABCDEFG"
        );
    }
}
