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

use std::fmt;

use indexmap::IndexMap;
use matrix_sdk::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{receipt::Receipt, AnySyncTimelineEvent},
    serde::Raw,
    OwnedEventId, OwnedUserId,
};

use super::BundledReactions;

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub(in crate::timeline) struct RemoteEventTimelineItem {
    /// The event ID.
    pub event_id: OwnedEventId,
    /// All bundled reactions about the event.
    pub reactions: BundledReactions,
    /// All read receipts for the event.
    ///
    /// The key is the ID of a room member and the value are details about the
    /// read receipt.
    ///
    /// Note that currently this ignores threads.
    pub read_receipts: IndexMap<OwnedUserId, Receipt>,
    /// Whether the event has been sent by the the logged-in user themselves.
    pub is_own: bool,
    /// Whether the item should be highlighted in the timeline.
    pub is_highlighted: bool,
    /// Encryption information.
    pub encryption_info: Option<EncryptionInfo>,
    /// JSON of the original event.
    ///
    /// If the event is edited, this *won't* change, instead `latest_edit_json`
    /// will be updated.
    ///
    /// This field always starts out as `Some(_)`, but is set to `None` when the
    /// event is redacted. The redacted form of the event could be computed
    /// locally instead (at least when the redaction came from the server and
    /// thus the whole event is available), but it's not clear whether there is
    /// a clear need for that.
    pub original_json: Option<Raw<AnySyncTimelineEvent>>,
    /// JSON of the latest edit to this item.
    pub latest_edit_json: Option<Raw<AnySyncTimelineEvent>>,
    /// Where we got this event from: A sync response or pagination.
    pub origin: RemoteEventOrigin,
}

impl RemoteEventTimelineItem {
    /// Clone the current event item, and update its `reactions`.
    pub fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, and clear its `reactions` as well as the
    /// JSON representation fields.
    pub fn redact(&self) -> Self {
        Self {
            reactions: BundledReactions::default(),
            original_json: None,
            latest_edit_json: None,
            ..self.clone()
        }
    }
}

/// Where we got an event from.
#[derive(Clone, Copy, Debug)]
pub(in crate::timeline) enum RemoteEventOrigin {
    /// The event came from a cache.
    Cache,
    /// The event came from a sync response.
    Sync,
    /// The event came from pagination.
    Pagination,
    /// We don't know.
    #[cfg(feature = "e2e-encryption")]
    Unknown,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RemoteEventTimelineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // skip raw JSON, too noisy
        let Self {
            event_id,
            reactions,
            read_receipts,
            is_own,
            encryption_info,
            original_json: _,
            latest_edit_json: _,
            is_highlighted,
            origin,
        } = self;

        f.debug_struct("RemoteEventTimelineItem")
            .field("event_id", event_id)
            .field("reactions", reactions)
            .field("read_receipts", read_receipts)
            .field("is_own", is_own)
            .field("is_highlighted", is_highlighted)
            .field("encryption_info", encryption_info)
            .field("origin", origin)
            .finish_non_exhaustive()
    }
}
