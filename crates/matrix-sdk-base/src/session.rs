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

use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};

/// A user session, containing an access token and information about the
/// associated user account.
///
/// # Example
///
/// ```
/// use matrix_sdk_base::Session;
/// use ruma::{device_id, user_id};
///
/// let session = Session {
///     access_token: "My-Token".to_owned(),
///     user_id: user_id!("@example:localhost").to_owned(),
///     device_id: device_id!("MYDEVICEID").to_owned(),
/// };
///
/// assert_eq!(session.device_id.as_str(), "MYDEVICEID");
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The access token used for this session.
    pub access_token: String,
    /// The user the access token was issued for.
    pub user_id: OwnedUserId,
    /// The ID of the client device
    pub device_id: OwnedDeviceId,
}

impl From<ruma::api::client::session::login::v3::Response> for Session {
    fn from(response: ruma::api::client::session::login::v3::Response) -> Self {
        Self {
            access_token: response.access_token,
            user_id: response.user_id,
            device_id: response.device_id,
        }
    }
}
