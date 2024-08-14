// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use indexmap::IndexMap;
use ruma::{events::relation::Annotation, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};

use super::event_item::TimelineEventItemId;

// Implements hash etc
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(super) struct AnnotationKey {
    event_id: OwnedEventId,
    key: String,
}

impl From<&Annotation> for AnnotationKey {
    fn from(annotation: &Annotation) -> Self {
        Self { event_id: annotation.event_id.clone(), key: annotation.key.clone() }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingReaction {
    /// The annotation used for the reaction.
    pub key: String,
    /// Sender identifier.
    pub sender_id: OwnedUserId,
    /// Date at which the sender reacted.
    pub timestamp: MilliSecondsSinceUnixEpoch,
}

#[derive(Clone, Debug)]
pub(crate) struct FullReactionKey {
    pub item: TimelineEventItemId,
    pub key: String,
    pub sender: OwnedUserId,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Reactions {
    /// Reaction event / txn ID => full path to the reaction in some item.
    pub map: HashMap<TimelineEventItemId, FullReactionKey>,
    /// Mapping of events that are not in the timeline => reaction event id =>
    /// pending reaction.
    pub pending: HashMap<OwnedEventId, IndexMap<OwnedEventId, PendingReaction>>,
}

impl Reactions {
    pub(super) fn clear(&mut self) {
        self.map.clear();
        self.pending.clear();
    }
}
