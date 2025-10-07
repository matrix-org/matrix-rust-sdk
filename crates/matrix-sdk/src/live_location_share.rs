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
//!
//! Live location sharing allows users to share their real-time location with
//! others in a room via [MSC3489](https://github.com/matrix-org/matrix-spec-proposals/pull/3489).
use async_stream::stream;
use futures_util::Stream;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedUserId, RoomId,
    events::{
        beacon::OriginalSyncBeaconEvent, beacon_info::BeaconInfoEventContent,
        location::LocationContent,
    },
};

use crate::{Client, Room, event_handler::ObservableEventHandler};

/// An observable live location.
#[derive(Debug)]
pub struct ObservableLiveLocation {
    observable_room_events: ObservableEventHandler<(OriginalSyncBeaconEvent, Room)>,
}

impl ObservableLiveLocation {
    /// Create a new `ObservableLiveLocation` for a particular room.
    pub fn new(client: &Client, room_id: &RoomId) -> Self {
        Self { observable_room_events: client.observe_room_events(room_id) }
    }

    /// Get a stream of [`LiveLocationShare`].
    pub fn subscribe(&self) -> impl Stream<Item = LiveLocationShare> + use<> {
        let stream = self.observable_room_events.subscribe();

        stream! {
            for await (event, room) in stream {
                if event.sender != room.own_user_id() {
                    yield LiveLocationShare {
                        last_location: LastLocation {
                            location: event.content.location,
                            ts: event.origin_server_ts,
                        },
                        beacon_info: room
                            .get_user_beacon_info(&event.sender)
                            .await
                            .ok()
                            .map(|info| info.content),
                        user_id: event.sender,
                    };
                }
            }
        }
    }
}

/// Details of the last known location beacon.
#[derive(Clone, Debug)]
pub struct LastLocation {
    /// The most recent location content of the user.
    pub location: LocationContent,
    /// The timestamp of when the location was updated.
    pub ts: MilliSecondsSinceUnixEpoch,
}

/// Details of a users live location share.
#[derive(Clone, Debug)]
pub struct LiveLocationShare {
    /// The user's last known location.
    pub last_location: LastLocation,
    /// Information about the associated beacon event.
    pub beacon_info: Option<BeaconInfoEventContent>,
    /// The user ID of the person sharing their live location.
    pub user_id: OwnedUserId,
}
