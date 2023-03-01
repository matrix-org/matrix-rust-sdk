use std::fmt;

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{receipt::Receipt, AnySyncTimelineEvent},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
};

use super::{BundledReactions, Profile, TimelineDetails, TimelineItemContent};

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub struct RemoteEventTimelineItem {
    /// The event ID.
    pub event_id: OwnedEventId,
    /// The sender of the event.
    pub sender: OwnedUserId,
    /// The sender's profile of the event.
    pub sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    pub content: TimelineItemContent,
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
    // FIXME: Expose the raw JSON of aggregated events somehow
    pub raw: Raw<AnySyncTimelineEvent>,
}

impl RemoteEventTimelineItem {
    /// Clone the current event item, and update its `reactions`.
    pub(in crate::room::timeline) fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, and update its `content`.
    pub(in crate::room::timeline) fn with_content(&self, content: TimelineItemContent) -> Self {
        Self { content, ..self.clone() }
    }

    /// Clone the current event item, change its `content` to
    /// [`TimelineItemContent::RedactedMessage`], and reset its `reactions`.
    pub(in crate::room::timeline) fn to_redacted(&self) -> Self {
        Self {
            // FIXME: Change when we support state events
            content: TimelineItemContent::RedactedMessage,
            reactions: BundledReactions::default(),
            ..self.clone()
        }
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &BundledReactions {
        // FIXME: Find out the state of incomplete bundled reactions, adjust
        //        Ruma if necessary, return the whole BundledReactions field
        &self.reactions
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RemoteEventTimelineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteEventTimelineItem")
            .field("event_id", &self.event_id)
            .field("sender", &self.sender)
            .field("timestamp", &self.timestamp)
            .field("content", &self.content)
            .field("reactions", &self.reactions)
            .field("is_own", &self.is_own)
            .field("encryption_info", &self.encryption_info)
            // skip raw, too noisy
            .finish_non_exhaustive()
    }
}
