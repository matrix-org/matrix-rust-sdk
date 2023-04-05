use std::fmt;

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{receipt::Receipt, AnySyncTimelineEvent},
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId,
};

use super::{BundledReactions, Profile, TimelineDetails, TimelineItemContent};

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub struct RemoteEventTimelineItem {
    /// The event ID.
    event_id: OwnedEventId,
    /// The sender of the event.
    sender: OwnedUserId,
    /// The sender's profile of the event.
    sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    content: TimelineItemContent,
    /// All bundled reactions about the event.
    reactions: BundledReactions,
    /// All read receipts for the event.
    ///
    /// The key is the ID of a room member and the value are details about the
    /// read receipt.
    ///
    /// Note that currently this ignores threads.
    read_receipts: IndexMap<OwnedUserId, Receipt>,
    /// Whether the event has been sent by the the logged-in user themselves.
    is_own: bool,
    /// Encryption information.
    encryption_info: Option<EncryptionInfo>,
    /// JSON of the original event.
    ///
    /// If the message is edited, this *won't* change, instead
    /// `latest_edit_json` will be updated.
    original_json: Raw<AnySyncTimelineEvent>,
    /// JSON of the latest edit to this item.
    latest_edit_json: Option<Raw<AnySyncTimelineEvent>>,
    /// Whether the item should be highlighted in the timeline.
    is_highlighted: bool,
}

impl RemoteEventTimelineItem {
    #[allow(clippy::too_many_arguments)] // Would be nice to fix, but unclear how
    pub(in crate::room::timeline) fn new(
        event_id: OwnedEventId,
        sender: OwnedUserId,
        sender_profile: TimelineDetails<Profile>,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: TimelineItemContent,
        reactions: BundledReactions,
        read_receipts: IndexMap<OwnedUserId, Receipt>,
        is_own: bool,
        encryption_info: Option<EncryptionInfo>,
        original_json: Raw<AnySyncTimelineEvent>,
        is_highlighted: bool,
    ) -> Self {
        Self {
            event_id,
            sender,
            sender_profile,
            timestamp,
            content,
            reactions,
            read_receipts,
            is_own,
            encryption_info,
            original_json,
            latest_edit_json: None,
            is_highlighted,
        }
    }

    /// Get the ID of the event.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    /// Get the sender of the event.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the event's sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    /// Get the event timestamp as set by the homeserver that created the event.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        self.timestamp
    }

    /// Get the content of the event.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &BundledReactions {
        // FIXME: Find out the state of incomplete bundled reactions, adjust
        //        Ruma if necessary, return the whole BundledReactions field
        &self.reactions
    }

    /// Get the read receipts of this item.
    ///
    /// The key is the ID of a room member and the value are details about the
    /// read receipt.
    ///
    /// Note that currently this ignores threads.
    pub fn read_receipts(&self) -> &IndexMap<OwnedUserId, Receipt> {
        &self.read_receipts
    }

    /// Whether the event has been sent by the the logged-in user themselves.
    pub fn is_own(&self) -> bool {
        self.is_own
    }

    /// Get the encryption information for the event.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        self.encryption_info.as_ref()
    }

    /// Get the raw JSON representation of the primary event.
    pub fn original_json(&self) -> &Raw<AnySyncTimelineEvent> {
        &self.original_json
    }

    /// Get the raw JSON representation of the latest edit, if any.
    pub fn latest_edit_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        self.latest_edit_json.as_ref()
    }

    /// Whether the event should be highlighted in the timeline.
    pub fn is_highlighted(&self) -> bool {
        self.is_highlighted
    }

    pub(in crate::room::timeline) fn set_content(&mut self, content: TimelineItemContent) {
        self.content = content;
    }

    pub(in crate::room::timeline) fn add_read_receipt(
        &mut self,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        self.read_receipts.insert(user_id, receipt);
    }

    /// Remove the read receipt for the given user.
    ///
    /// Returns `true` if there was one, `false` if not.
    pub(in crate::room::timeline) fn remove_read_receipt(&mut self, user_id: &UserId) -> bool {
        self.read_receipts.remove(user_id).is_some()
    }

    /// Clone the current event item, and update its `reactions`.
    pub(in crate::room::timeline) fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, and update its `content`.
    pub(in crate::room::timeline) fn apply_edit(
        &self,
        content: TimelineItemContent,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) -> Self {
        // If the edit is local (is not a full event yet), `edit_json` will be
        // `None`, in that case retain the existing value of `latest_edit_json`
        let latest_edit_json = edit_json.or_else(|| self.latest_edit_json.clone());
        Self { content, latest_edit_json, ..self.clone() }
    }

    /// Clone the current event item, and update its `sender_profile`.
    pub(in crate::room::timeline) fn with_sender_profile(
        &self,
        sender_profile: TimelineDetails<Profile>,
    ) -> Self {
        Self { sender_profile, ..self.clone() }
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
