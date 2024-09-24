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

//! Types for live location sharing.

use ruma::{events::location::LocationContent, MilliSecondsSinceUnixEpoch, OwnedUserId};

/// Details of the last known location beacon.
#[derive(Clone, Debug)]
pub struct LastLocation {
    /// The most recent location content of the user.
    pub location: LocationContent,
    /// The timestamp of when the location was updated
    pub ts: MilliSecondsSinceUnixEpoch,
}

/// Details of a users live location share.
#[derive(Clone, Debug)]
pub struct LiveLocationShare {
    /// The user's last known location.
    pub last_location: LastLocation,
    // /// Information about the associated beacon event (currently commented out).
    // pub beacon_info: BeaconInfoEventContent,
    /// The user ID of the person sharing their live location.
    pub user_id: OwnedUserId,
}
