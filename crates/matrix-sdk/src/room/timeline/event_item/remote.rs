use std::fmt;

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{receipt::Receipt, AnySyncTimelineEvent},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId,
};

use super::BundledReactions;

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub(in crate::room::timeline) struct RemoteEventTimelineItem {
    /// The event ID.
    pub event_id: OwnedEventId,
    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,
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
    /// Encryption information.
    pub encryption_info: Option<EncryptionInfo>,
    /// JSON of the original event.
    ///
    /// If the message is edited, this *won't* change, instead
    /// `latest_edit_json` will be updated.
    pub original_json: Raw<AnySyncTimelineEvent>,
    /// JSON of the latest edit to this item.
    pub latest_edit_json: Option<Raw<AnySyncTimelineEvent>>,
    /// Whether the item should be highlighted in the timeline.
    pub is_highlighted: bool,
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

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RemoteEventTimelineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteEventTimelineItem")
            .field("event_id", &self.event_id)
            .field("timestamp", &self.timestamp)
            .field("reactions", &self.reactions)
            .field("is_own", &self.is_own)
            .field("encryption_info", &self.encryption_info)
            // skip raw, too noisy
            .finish_non_exhaustive()
    }
}
