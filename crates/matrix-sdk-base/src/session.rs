// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! User sessions.

use std::fmt;

use ruma::{api::client::session::refresh_token, OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};

/// A user session, containing an access token, an optional refresh token and
/// information about the associated user account.
///
/// # Examples
///
/// ```
/// use matrix_sdk_base::Session;
/// use ruma::{device_id, user_id};
///
/// let session = Session {
///     access_token: "My-Token".to_owned(),
///     refresh_token: None,
///     user_id: user_id!("@example:localhost").to_owned(),
///     device_id: device_id!("MYDEVICEID").to_owned(),
/// };
///
/// assert_eq!(session.device_id.as_str(), "MYDEVICEID");
/// ```
#[derive(Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The access token used for this session.
    pub access_token: String,
    /// The token used for [refreshing the access token], if any.
    ///
    /// [refreshing the access token]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The user the access token was issued for.
    pub user_id: OwnedUserId,
    /// The ID of the client device.
    pub device_id: OwnedDeviceId,
}

impl Session {
    /// Creates a `Session` from a `SessionMeta` and `SessionTokens`.
    pub fn from_parts(meta: SessionMeta, tokens: SessionTokens) -> Self {
        let SessionMeta { user_id, device_id } = meta;
        let SessionTokens { access_token, refresh_token } = tokens;
        Self { access_token, refresh_token, user_id, device_id }
    }

    /// Split this `Session` between `SessionMeta` and `SessionTokens`.
    pub fn into_parts(self) -> (SessionMeta, SessionTokens) {
        let Self { access_token, refresh_token, user_id, device_id } = self;
        (SessionMeta { user_id, device_id }, SessionTokens { access_token, refresh_token })
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl From<ruma::api::client::session::login::v3::Response> for Session {
    fn from(response: ruma::api::client::session::login::v3::Response) -> Self {
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            user_id: response.user_id,
            device_id: response.device_id,
        }
    }
}

/// The immutable parts of the session: the user ID and device ID.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    /// The user the access token was issued for.
    pub user_id: OwnedUserId,
    /// The ID of the client device.
    pub device_id: OwnedDeviceId,
}

/// The mutable parts of the session: the access token and optional refresh
/// token.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct SessionTokens {
    /// The access token used for this session.
    pub access_token: String,
    /// The token used for [refreshing the access token], if any.
    ///
    /// [refreshing the access token]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub refresh_token: Option<String>,
}

impl SessionTokens {
    /// Update this `SessionTokens` with the values found in the given response.
    pub fn update_with_refresh_response(&mut self, response: &refresh_token::v3::Response) {
        self.access_token = response.access_token.clone();
        if let Some(refresh_token) = response.refresh_token.clone() {
            self.refresh_token = Some(refresh_token);
        }
    }
}
