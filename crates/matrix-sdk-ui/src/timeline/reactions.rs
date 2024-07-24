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
use ruma::{
    events::relation::Annotation, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId,
    OwnedUserId,
};

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

#[derive(Debug, Clone)]
pub(super) enum ReactionAction {
    /// Request already in progress so allow that one to resolve
    None,

    /// Send this reaction to the server
    SendRemote(OwnedTransactionId),

    /// Redact this reaction from the server
    RedactRemote(OwnedEventId),
}

#[derive(Debug, Clone)]
pub(super) enum ReactionState {
    /// We're redacting a reaction.
    ///
    /// The optional event id is defined if, and only if, there already was a
    /// remote echo for this reaction.
    Redacting(Option<OwnedEventId>),
    /// We're sending the reaction with the given transaction id, which we'll
    /// use to match against the response in the sync event.
    Sending(OwnedTransactionId),
}

/// Data associated with a reaction sender. It can be used to display
/// a details UI component for a reaction with both sender
/// names and the date at which they sent a reaction.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReactionSenderData {
    /// Sender identifier.
    pub sender_id: OwnedUserId,
    /// Date at which the sender reacted.
    pub timestamp: MilliSecondsSinceUnixEpoch,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingReaction {
    pub key: String,
    pub sender_data: ReactionSenderData,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Reactions {
    /// Reaction event / txn ID => sender and reaction data.
    pub map: HashMap<TimelineEventItemId, (ReactionSenderData, Annotation)>,
    /// Mapping of events that are not in the timeline => reaction event id =>
    /// pending reaction.
    pub pending: HashMap<OwnedEventId, IndexMap<OwnedEventId, PendingReaction>>,
    /// The local reaction request state that is queued next.
    pub reaction_state: IndexMap<AnnotationKey, ReactionState>,
    /// The in-flight reaction request state that is ongoing.
    pub in_flight_reaction: IndexMap<AnnotationKey, ReactionState>,
}

impl Reactions {
    pub(super) fn clear(&mut self) {
        self.map.clear();
        self.pending.clear();
        self.reaction_state.clear();
        self.in_flight_reaction.clear();
    }
}

/// The result of toggling a reaction
///
/// Holds the data required to update the state of the reaction in the timeline
#[derive(Clone, Debug)]
pub(super) enum ReactionToggleResult {
    /// Represents a successful reaction toggle which added a reaction
    AddSuccess {
        /// The event ID of the reaction which was added (the remote echo)
        event_id: OwnedEventId,

        /// The transaction ID of the reaction which was added (the local echo)
        txn_id: OwnedTransactionId,
    },

    /// Represents a failed reaction toggle which did not add a reaction
    AddFailure {
        /// The transaction ID of the reaction which failed to be added (the
        /// local echo)
        txn_id: OwnedTransactionId,
    },

    /// Represents a successful reaction toggle which redacted a reaction
    RedactSuccess,

    /// Represents a failed reaction toggle which did not redact a reaction
    RedactFailure {
        /// The event ID of the reaction which failed to be redacted (the remote
        /// echo)
        event_id: OwnedEventId,
    },
}
