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

use std::{collections::BTreeMap, time::Duration};

use ruma::{OwnedDeviceId, OwnedUserId};

/// Customized version of `ruma_client_api::keys::get_keys::v3::Request`,
/// without any references.
#[derive(Clone, Debug)]
pub struct KeysQueryRequest {
    /// The time (in milliseconds) to wait when downloading keys from remote
    /// servers. 10 seconds is the recommended default.
    pub timeout: Option<Duration>,

    /// The keys to be downloaded. An empty list indicates all devices for
    /// the corresponding user.
    pub device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
}

impl KeysQueryRequest {
    pub(crate) fn new(users: impl Iterator<Item = OwnedUserId>) -> Self {
        let device_keys = users.map(|u| (u, Vec::new())).collect();

        Self { timeout: None, device_keys }
    }
}
