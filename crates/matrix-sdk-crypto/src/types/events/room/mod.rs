// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Types for room events.

use std::{collections::BTreeMap, fmt::Debug};

use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::EventType;

pub mod encrypted;

/// Generic room event with a known type and content.
#[derive(Debug, Deserialize)]
pub struct Event<C>
where
    C: EventType + Debug + Sized + Serialize,
{
    /// Contains the fully-qualified ID of the user who sent this event.
    pub sender: OwnedUserId,

    /// The globally unique identifier for this event.
    pub event_id: OwnedEventId,

    /// Present if and only if this event is a state event.
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub state_key: Option<String>,

    /// The body of this event, as created by the client which sent it.
    pub content: C,

    /// Timestamp (in milliseconds since the unix epoch) on originating
    /// homeserver when this event was sent.
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,

    /// Contains optional extra information about the event.
    #[serde(default)]
    pub unsigned: BTreeMap<String, Value>,

    /// Any other unknown data of the room event.
    #[serde(flatten)]
    pub(crate) other: BTreeMap<String, Value>,
}

impl<C> Serialize for Event<C>
where
    C: EventType + Debug + Sized + Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a, C> {
            sender: &'a UserId,
            event_id: &'a EventId,
            #[serde(rename = "type")]
            event_type: &'a str,
            content: &'a C,
            origin_server_ts: MilliSecondsSinceUnixEpoch,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            unsigned: &'a BTreeMap<String, Value>,
            #[serde(flatten)]
            other: &'a BTreeMap<String, Value>,
        }

        let event_type = C::EVENT_TYPE;

        let helper = Helper {
            sender: &self.sender,
            content: &self.content,
            event_type,
            other: &self.other,
            event_id: &self.event_id,
            origin_server_ts: self.origin_server_ts,
            unsigned: &self.unsigned,
        };

        helper.serialize(serializer)
    }
}
