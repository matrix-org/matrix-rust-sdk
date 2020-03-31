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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::UserId;
use crate::api::r0 as api;
use crate::events::collections::all::{Event, RoomEvent, StateEvent};
use crate::events::presence::{PresenceEvent, PresenceEventContent, PresenceState};
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    member::{MemberEvent, MembershipState},
    name::NameEvent,
};
use crate::events::EventResult;
use crate::identifiers::RoomAliasId;
use crate::session::Session;

use js_int::UInt;
#[cfg(feature = "encryption")]
use tokio::sync::Mutex;

#[cfg(feature = "encryption")]
use crate::crypto::{OlmMachine, OneTimeKeys};
#[cfg(feature = "encryption")]
use ruma_client_api::r0::keys::{upload_keys::Response as KeysUploadResponse, DeviceKeys};

#[derive(Debug)]
/// A Matrix room member.
pub struct User {
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The presence of the user, if found.
    pub presence: Option<PresenceState>,
    /// The presence status message, if found.
    pub status_msg: Option<String>,
    /// The time, in ms, since the user interacted with the server.
    pub last_active_ago: Option<UInt>,
    /// If the user should be considered active.
    pub currently_active: Option<bool>,
    /// The events that created the state of the current user.
    // TODO do we want to hold the whole state or just update our structures.
    pub events: Vec<Event>,
    /// The `PresenceEvent`s connected to this user.
    pub presence_events: Vec<PresenceEvent>,
}

impl User {
    pub fn new(event: &MemberEvent) -> Self {
        Self {
            display_name: event.content.displayname.clone(),
            avatar_url: event.content.avatar_url.clone(),
            presence: None,
            status_msg: None,
            last_active_ago: None,
            currently_active: None,
            events: Vec::default(),
            presence_events: Vec::default(),
        }
    }

    /// If the current `PresenceEvent` updated the state of this `User`.
    ///
    /// Returns true if the specific users presence has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `presence` - The presence event for a this room member.
    pub fn did_update_presence(&self, presence: &PresenceEvent) -> bool {
        let PresenceEvent {
            content:
                PresenceEventContent {
                    avatar_url,
                    currently_active,
                    displayname,
                    last_active_ago,
                    presence,
                    status_msg,
                },
            ..
        } = presence;
        self.display_name == *displayname
            && self.avatar_url == *avatar_url
            && self.presence.as_ref() == Some(presence)
            && self.status_msg == *status_msg
            && self.last_active_ago == *last_active_ago
            && self.currently_active == *currently_active
    }

    /// Updates the `User`s presence.
    ///
    /// This should only be used if `did_update_presence` was true.
    ///
    /// # Arguments
    ///
    /// * `presence` - The presence event for a this room member.
    pub fn update_presence(&mut self, presence_ev: &PresenceEvent) {
        let PresenceEvent {
            content:
                PresenceEventContent {
                    avatar_url,
                    currently_active,
                    displayname,
                    last_active_ago,
                    presence,
                    status_msg,
                },
            ..
        } = presence_ev;

        self.presence_events.push(presence_ev.clone());
        *self = User {
            display_name: displayname.clone(),
            avatar_url: avatar_url.clone(),
            presence: Some(*presence),
            status_msg: status_msg.clone(),
            last_active_ago: *last_active_ago,
            currently_active: *currently_active,
            // TODO better way of moving vec over
            events: self.events.clone(),
            presence_events: self.presence_events.clone(),
        }
    }
}
