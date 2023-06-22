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

use ruma::{EventId, OwnedEventId, OwnedTransactionId, TransactionId};

#[derive(Clone, Debug)]
/// The result of toggling a reaction
///
/// Holds the data required to update the state of the reaction in the timeline
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

impl ReactionToggleResult {
    /// Construct a new `ReactionToggleResult` representing a successful
    /// reaction toggle which added a reaction
    pub(super) fn add_success(event_id: &EventId, txn_id: &TransactionId) -> Self {
        Self::AddSuccess { event_id: event_id.to_owned(), txn_id: txn_id.to_owned() }
    }

    /// Construct a new `ReactionToggleResult` representing a successful  
    /// reaction toggle which redacted a reaction
    pub(super) fn redact_success() -> Self {
        Self::RedactSuccess {}
    }

    /// Construct a new `ReactionToggleResult` representing a failed attempt
    /// to add a reaction
    pub(super) fn add_failure(txn_id: &TransactionId) -> Self {
        Self::AddFailure { txn_id: txn_id.to_owned() }
    }

    /// Construct a new `ReactionToggleResult` representing a failed attempt
    /// to redact a reaction
    pub(super) fn redact_failure(event_id: &EventId) -> Self {
        Self::RedactFailure { event_id: event_id.to_owned() }
    }
}
