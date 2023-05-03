use std::fmt;

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{receipt::Receipt, AnySyncTimelineEvent},
    serde::Raw,
    OwnedEventId, OwnedUserId, UserId,
};

use super::BundledReactions;

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub(in crate::room::timeline) struct RemoteEventTimelineItem {
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
    /// If the message is edited, this *won't* change, instead
    /// `latest_edit_json` will be updated.
    pub original_json: Raw<AnySyncTimelineEvent>,
    /// JSON of the latest edit to this item.
    pub latest_edit_json: Option<Raw<AnySyncTimelineEvent>>,
    /// Where we got this event from: A sync response or pagination.
    pub origin: RemoteEventOrigin,
}

impl RemoteEventTimelineItem {
    pub fn add_read_receipt(&mut self, user_id: OwnedUserId, receipt: Receipt) {
        self.read_receipts.insert(user_id, receipt);
    }

    /// Remove the read receipt for the given user.
    ///
    /// Returns `true` if there was one, `false` if not.
    pub fn remove_read_receipt(&mut self, user_id: &UserId) -> bool {
        self.read_receipts.remove(user_id).is_some()
    }

    /// Clone the current event item, and update its `reactions`.
    pub fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, change its `content` to
    /// [`TimelineItemContent::RedactedMessage`], and reset its `reactions`.
    pub fn to_redacted(&self) -> Self {
        Self { reactions: BundledReactions::default(), ..self.clone() }
    }
}

/// Where we got an event from.
#[derive(Clone, Copy, Debug)]
pub(in crate::room::timeline) enum RemoteEventOrigin {
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
