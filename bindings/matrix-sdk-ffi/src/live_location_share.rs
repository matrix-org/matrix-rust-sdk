// Copyright 2024 The Matrix.org Foundation C.I.C.
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
use crate::ruma::LocationContent;
#[derive(uniffi::Record)]
pub struct LastLocation {
    /// The most recent location content of the user.
    pub location: LocationContent,
    /// A timestamp in milliseconds since Unix Epoch on that day in local
    /// time.
    pub ts: u64,
}
/// Details of a users live location share.
#[derive(uniffi::Record)]
pub struct LiveLocationShare {
    /// The user's last known location.
    pub last_location: LastLocation,
    /// The live status of the live location share.
    pub(crate) is_live: bool,
    /// The user ID of the person sharing their live location.
    pub user_id: String,
}
