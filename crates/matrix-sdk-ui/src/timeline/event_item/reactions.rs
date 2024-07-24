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

use indexmap::IndexMap;
use itertools::Itertools as _;
use ruma::{OwnedEventId, OwnedTransactionId, UserId};

use super::TimelineEventItemId;
use crate::timeline::ReactionSenderData;

/// The reactions grouped by key.
///
/// Key: The reaction, usually an emoji.
/// Value: The group of reactions.
pub type BundledReactions = IndexMap<String, ReactionGroup>;

/// A group of reaction events on the same event with the same key.
///
/// This is a map of the event ID or transaction ID of the reactions to the ID
/// of the sender of the reaction.
#[derive(Clone, Debug, Default)]
pub struct ReactionGroup(pub(in crate::timeline) IndexMap<TimelineEventItemId, ReactionSenderData>);

impl ReactionGroup {
    /// The (deduplicated) senders of the reactions in this group.
    pub fn senders(&self) -> impl Iterator<Item = &ReactionSenderData> {
        self.0.values().unique_by(|v| &v.sender_id)
    }

    /// Returns the number of reactions in this group.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// All reactions within this reaction group that were sent by the given
    /// user.
    ///
    /// Note that it is possible for multiple reactions by the same user to
    /// have arrived over federation.
    pub fn by_sender<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> impl Iterator<Item = (Option<&OwnedTransactionId>, Option<&OwnedEventId>)> + 'a {
        self.0.iter().filter_map(move |(k, v)| {
            (v.sender_id == user_id).then_some(match k {
                TimelineEventItemId::TransactionId(txn_id) => (Some(txn_id), None),
                TimelineEventItemId::EventId(event_id) => (None, Some(event_id)),
            })
        })
    }
}
