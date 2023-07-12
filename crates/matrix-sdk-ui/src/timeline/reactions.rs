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

use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId};

/// Data associated with a reaction sender. It can be used to display
/// a details UI component for a reaction with both sender
/// names and the date at which they sent a reaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReactionSenderData {
    /// Sender identifier.
    pub sender_id: OwnedUserId,
    /// Date at which the sender reacted.
    pub timestamp: MilliSecondsSinceUnixEpoch,
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
