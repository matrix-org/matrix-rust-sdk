// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use as_variant::as_variant;
use eyeball_im::{ObservableVectorTransaction, ObservableVectorTransactionEntry};
use indexmap::IndexMap;
use matrix_sdk::{
    crypto::types::events::UtdCause, deserialized_responses::EncryptionInfo,
    ring_buffer::RingBuffer, send_queue::SendHandle,
};
use ruma::{
    events::{
        poll::{
            unstable_end::UnstablePollEndEventContent,
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::{
                NewUnstablePollStartEventContent, NewUnstablePollStartEventContentWithoutRelation,
                UnstablePollStartEventContent,
            },
        },
        reaction::ReactionEventContent,
        receipt::Receipt,
        relation::Replacement,
        room::{
            member::RoomMemberEventContent,
            message::{self, RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
        },
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, BundledMessageLikeRelations, EventContent, FullStateEventContent,
        MessageLikeEventType, StateEventType, SyncStateEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId,
};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};

use super::{
    controller::{PendingEditKind, TimelineMetadata, TimelineStateTransaction},
    day_dividers::DayDividerAdjuster,
    event_item::{
        extract_bundled_edit_event_json, extract_poll_edit_content, extract_room_msg_edit_content,
        AnyOtherFullStateEventContent, EventSendState, EventTimelineItemKind,
        LocalEventTimelineItem, PollState, Profile, ReactionsByKeyBySender, RemoteEventOrigin,
        RemoteEventTimelineItem, TimelineEventItemId,
    },
    reactions::FullReactionKey,
    util::{rfind_event_by_id, rfind_event_item},
    EventTimelineItem, InReplyToDetails, OtherState, Sticker, TimelineDetails, TimelineItem,
    TimelineItemContent,
};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        controller::PendingEdit,
        event_item::{ReactionInfo, ReactionStatus},
        reactions::PendingReaction,
        RepliedToEvent,
    },
};

/// When adding an event, useful information related to the source of the event.
pub(super) enum Flow {
    /// The event was locally created.
    Local {
        /// The transaction id we've used in requests associated to this event.
        txn_id: OwnedTransactionId,

        /// A handle to manipulate this event.
        send_handle: Option<SendHandle>,
    },

    /// The event has been received from a remote source (sync, pagination,
    /// etc.). This can be a "remote echo".
    Remote {
        /// The event identifier as returned by the server.
        event_id: OwnedEventId,
        /// The transaction id we might have used, if we're the sender of the
        /// event.
        txn_id: Option<OwnedTransactionId>,
        /// The raw serialized JSON event.
        raw_event: Raw<AnySyncTimelineEvent>,
        /// Where should this be added in the timeline.
        position: TimelineItemPosition,
        /// Information about the encryption for this event.
        encryption_info: Option<EncryptionInfo>,
    },
}

impl Flow {
    /// If the flow is remote, returns the associated event id.
    pub(crate) fn event_id(&self) -> Option<&EventId> {
        as_variant!(self, Flow::Remote { event_id, .. } => event_id)
    }

    /// If the flow is remote, returns the associated full raw event.
    pub(crate) fn raw_event(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        as_variant!(self, Flow::Remote { raw_event, .. } => raw_event)
    }
}

pub(super) struct TimelineEventContext {
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: Option<Profile>,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) is_own_event: bool,
    pub(super) read_receipts: IndexMap<OwnedUserId, Receipt>,
    pub(super) is_highlighted: bool,
    pub(super) flow: Flow,

    /// If the event represents a new item, should it be added to the timeline?
    ///
    /// This controls whether a new timeline *may* be added. If the update kind
    /// is about an update to an existing timeline item (redaction, edit,
    /// reaction, etc.), it's always handled by default.
    pub(super) should_add_new_items: bool,
}

#[derive(Clone, Debug)]
pub(super) enum TimelineEventKind {
    /// The common case: a message-like item.
    Message {
        content: AnyMessageLikeEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
    },

    /// Some remote event that was redacted a priori, i.e. we never had the
    /// original content, so we'll just display a dummy redacted timeline
    /// item.
    RedactedMessage { event_type: MessageLikeEventType },

    /// We're redacting a remote event that we may or may not know about (i.e.
    /// the redacted event *may* have a corresponding timeline item).
    Redaction { redacts: OwnedEventId },

    /// A timeline event for a room membership update.
    RoomMember {
        user_id: OwnedUserId,
        content: FullStateEventContent<RoomMemberEventContent>,
        sender: OwnedUserId,
    },

    /// A state update that's not a [`Self::RoomMember`] event.
    OtherState { state_key: String, content: AnyOtherFullStateEventContent },

    /// If the timeline is configured to display events that failed to parse, a
    /// special item indicating a message-like event that couldn't be
    /// deserialized.
    FailedToParseMessageLike { event_type: MessageLikeEventType, error: Arc<serde_json::Error> },

    /// If the timeline is configured to display events that failed to parse, a
    /// special item indicating a state event that couldn't be deserialized.
    FailedToParseState {
        event_type: StateEventType,
        state_key: String,
        error: Arc<serde_json::Error>,
    },
}

impl TimelineEventKind {
    /// Creates a new `TimelineEventKind` with the given event and room version.
    pub fn from_event(event: AnySyncTimelineEvent, room_version: &RoomVersionId) -> Self {
        match event {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) => {
                if let Some(redacts) = ev.redacts(room_version).map(ToOwned::to_owned) {
                    Self::Redaction { redacts }
                } else {
                    Self::RedactedMessage { event_type: ev.event_type() }
                }
            }
            AnySyncTimelineEvent::MessageLike(ev) => match ev.original_content() {
                Some(content) => Self::Message { content, relations: ev.relations() },
                None => Self::RedactedMessage { event_type: ev.event_type() },
            },
            AnySyncTimelineEvent::State(ev) => match ev {
                AnySyncStateEvent::RoomMember(ev) => match ev {
                    SyncStateEvent::Original(ev) => Self::RoomMember {
                        user_id: ev.state_key,
                        content: FullStateEventContent::Original {
                            content: ev.content,
                            prev_content: ev.unsigned.prev_content,
                        },
                        sender: ev.sender,
                    },
                    SyncStateEvent::Redacted(ev) => Self::RoomMember {
                        user_id: ev.state_key,
                        content: FullStateEventContent::Redacted(ev.content),
                        sender: ev.sender,
                    },
                },
                ev => Self::OtherState {
                    state_key: ev.state_key().to_owned(),
                    content: AnyOtherFullStateEventContent::with_event_content(ev.content()),
                },
            },
        }
    }

    pub(super) fn failed_to_parse(
        event: SyncTimelineEventWithoutContent,
        error: serde_json::Error,
    ) -> Self {
        let error = Arc::new(error);
        match event {
            SyncTimelineEventWithoutContent::OriginalMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::RedactedMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::OriginalState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
            SyncTimelineEventWithoutContent::RedactedState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
        }
    }
}

/// The position at which to perform an update of the timeline with events.
#[derive(Clone, Copy, Debug)]
pub(super) enum TimelineItemPosition {
    /// One or more items are prepended to the timeline (i.e. they're the
    /// oldest).
    Start { origin: RemoteEventOrigin },

    /// One or more items are appended to the timeline (i.e. they're the most
    /// recent).
    End { origin: RemoteEventOrigin },

    /// A single item is updated.
    ///
    /// This only happens when a UTD must be replaced with the decrypted event.
    Update(usize),
}

/// The outcome of handling a single event with
/// [`TimelineEventHandler::handle_event`].
#[derive(Default)]
pub(super) struct HandleEventResult {
    /// Was some timeline item added?
    pub(super) item_added: bool,

    /// Was some timeline item removed?
    ///
    /// This can happen only if there was a UTD item that has been decrypted
    /// into an item that was filtered out with the event filter.
    pub(super) item_removed: bool,

    /// How many items were updated?
    pub(super) items_updated: u16,
}

/// Data necessary to update the timeline, given a single event to handle.
///
/// Bundles together a few things that are needed throughout the different
/// stages of handling an event (figuring out whether it should update an
/// existing timeline item, transforming that item or creating a new one,
/// updating the reactive Vec).
pub(super) struct TimelineEventHandler<'a, 'o> {
    items: &'a mut ObservableVectorTransaction<'o, Arc<TimelineItem>>,
    meta: &'a mut TimelineMetadata,
    ctx: TimelineEventContext,
    result: HandleEventResult,
}

impl<'a, 'o> TimelineEventHandler<'a, 'o> {
    pub(super) fn new(
        state: &'a mut TimelineStateTransaction<'o>,
        ctx: TimelineEventContext,
    ) -> Self {
        let TimelineStateTransaction { items, meta, .. } = state;
        Self { items, meta, ctx, result: HandleEventResult::default() }
    }

    /// Handle an event.
    ///
    /// Returns the number of timeline updates that were made.
    ///
    /// `raw_event` is only needed to determine the cause of any UTDs,
    /// so if we know this is not a UTD it can be None.
    #[instrument(skip_all, fields(txn_id, event_id, position))]
    pub(super) async fn handle_event(
        mut self,
        day_divider_adjuster: &mut DayDividerAdjuster,
        event_kind: TimelineEventKind,
    ) -> HandleEventResult {
        let span = tracing::Span::current();

        day_divider_adjuster.mark_used();

        match &self.ctx.flow {
            Flow::Local { txn_id, .. } => {
                span.record("txn_id", debug(txn_id));
                debug!("Handling local event");
            }

            Flow::Remote { event_id, txn_id, position, .. } => {
                span.record("event_id", debug(event_id));
                span.record("position", debug(position));
                if let Some(txn_id) = txn_id {
                    span.record("txn_id", debug(txn_id));
                }
                trace!("Handling remote event");
            }
        };

        let should_add = self.ctx.should_add_new_items;

        match event_kind {
            TimelineEventKind::Message { content, relations } => match content {
                AnyMessageLikeEventContent::Reaction(c) => {
                    self.handle_reaction(c);
                }

                AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent {
                    relates_to: Some(message::Relation::Replacement(re)),
                    ..
                }) => {
                    self.handle_room_message_edit(re);
                }

                AnyMessageLikeEventContent::RoomMessage(c) => {
                    if should_add {
                        self.handle_room_message(c, relations);
                    }
                }

                AnyMessageLikeEventContent::RoomEncrypted(c) => {
                    // TODO: Handle replacements if the replaced event is also UTD
                    let raw_event = self.ctx.flow.raw_event();
                    let cause = UtdCause::determine(raw_event);
                    self.add_item(TimelineItemContent::unable_to_decrypt(c, cause), None);

                    // Let the hook know that we ran into an unable-to-decrypt that is added to the
                    // timeline.
                    if let Some(hook) = self.meta.unable_to_decrypt_hook.as_ref() {
                        if let Some(event_id) = &self.ctx.flow.event_id() {
                            hook.on_utd(event_id, cause).await;
                        }
                    }
                }

                AnyMessageLikeEventContent::Sticker(content) => {
                    if should_add {
                        self.add_item(TimelineItemContent::Sticker(Sticker { content }), None);
                    }
                }

                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::Replacement(c),
                ) => self.handle_poll_edit(c.relates_to),

                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::New(c),
                ) => {
                    if should_add {
                        self.handle_poll_start(c, relations)
                    }
                }

                AnyMessageLikeEventContent::UnstablePollResponse(c) => self.handle_poll_response(c),

                AnyMessageLikeEventContent::UnstablePollEnd(c) => self.handle_poll_end(c),

                AnyMessageLikeEventContent::CallInvite(_) => {
                    if should_add {
                        self.add_item(TimelineItemContent::CallInvite, None);
                    }
                }

                AnyMessageLikeEventContent::CallNotify(_) => {
                    if should_add {
                        self.add_item(TimelineItemContent::CallNotify, None)
                    }
                }

                // TODO
                _ => {
                    debug!(
                        "Ignoring message-like event of type `{}`, not supported (yet)",
                        content.event_type()
                    );
                }
            },

            TimelineEventKind::RedactedMessage { event_type } => {
                if event_type != MessageLikeEventType::Reaction && should_add {
                    self.add_item(TimelineItemContent::RedactedMessage, None);
                }
            }

            TimelineEventKind::Redaction { redacts } => {
                self.handle_redaction(redacts);
            }

            TimelineEventKind::RoomMember { user_id, content, sender } => {
                if should_add {
                    self.add_item(TimelineItemContent::room_member(user_id, content, sender), None);
                }
            }

            TimelineEventKind::OtherState { state_key, content } => {
                // Update room encryption if a `m.room.encryption` event is found in the
                // timeline
                if should_add {
                    self.add_item(
                        TimelineItemContent::OtherState(OtherState { state_key, content }),
                        None,
                    );
                }
            }

            TimelineEventKind::FailedToParseMessageLike { event_type, error } => {
                if should_add {
                    self.add_item(
                        TimelineItemContent::FailedToParseMessageLike { event_type, error },
                        None,
                    );
                }
            }

            TimelineEventKind::FailedToParseState { event_type, state_key, error } => {
                if should_add {
                    self.add_item(
                        TimelineItemContent::FailedToParseState { event_type, state_key, error },
                        None,
                    );
                }
            }
        }

        if !self.result.item_added {
            trace!("No new item added");

            if let Flow::Remote { position: TimelineItemPosition::Update(idx), .. } = self.ctx.flow
            {
                // If add was not called, that means the UTD event is one that
                // wouldn't normally be visible. Remove it.
                trace!("Removing UTD that was successfully retried");
                self.items.remove(idx);

                self.result.item_removed = true;
            }

            // TODO: Add event as raw
        }

        self.result
    }

    /// Handles a new room message by adding it to the timeline.
    #[instrument(skip_all)]
    fn handle_room_message(
        &mut self,
        msg: RoomMessageEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
    ) {
        // Always remove the pending edit, if there's any. The reason is that if
        // there's an edit in the relations mapping, we want to prefer it over any
        // other pending edit, since it's more likely to be up to date, and we
        // don't want to apply another pending edit on top of it.
        let pending_edit = self
            .ctx
            .flow
            .event_id()
            .and_then(|event_id| {
                Self::maybe_unstash_pending_edit(&mut self.meta.pending_edits, event_id)
            })
            .and_then(|edit| match edit.kind {
                PendingEditKind::RoomMessage(replacement) => {
                    Some((Some(edit.event_json), replacement.new_content))
                }
                _ => None,
            });

        let (edit_json, edit_content) = extract_room_msg_edit_content(relations)
            .map(|content| {
                let edit_json = self.ctx.flow.raw_event().and_then(extract_bundled_edit_event_json);
                (edit_json, content)
            })
            .or(pending_edit)
            .unzip();

        let edit_json = edit_json.flatten();

        self.add_item(TimelineItemContent::message(msg, edit_content, self.items), edit_json);
    }

    #[instrument(skip_all, fields(replacement_event_id = ?replacement.event_id))]
    fn handle_room_message_edit(
        &mut self,
        replacement: Replacement<RoomMessageEventContentWithoutRelation>,
    ) {
        if let Some((item_pos, item)) = rfind_event_by_id(self.items, &replacement.event_id) {
            let edit_json = self.ctx.flow.raw_event().cloned();
            if let Some(new_item) = self.apply_msg_edit(&item, replacement.new_content, edit_json) {
                trace!("Applied edit");

                let internal_id = item.internal_id.to_owned();

                // Update all events that replied to this message with the edited content.
                self.items.for_each(|mut entry| {
                    let Some(event_item) = entry.as_event() else { return };
                    let Some(message) = event_item.content.as_message() else { return };
                    let Some(in_reply_to) = message.in_reply_to() else { return };
                    if replacement.event_id == in_reply_to.event_id {
                        trace!(reply_event_id = ?event_item.identifier(), "Updating response to edited event");
                        let in_reply_to = InReplyToDetails {
                            event_id: in_reply_to.event_id.clone(),
                            event: TimelineDetails::Ready(Box::new(
                                RepliedToEvent::from_timeline_item(&new_item),
                            )),
                        };
                        let new_reply_content =
                            TimelineItemContent::Message(message.with_in_reply_to(in_reply_to));
                        let new_reply_item =
                            entry.with_kind(event_item.with_content(new_reply_content, None));
                        ObservableVectorTransactionEntry::set(&mut entry, new_reply_item);
                    }
                });

                // Update the event itself.
                self.items.set(item_pos, TimelineItem::new(new_item, internal_id));
                self.result.items_updated += 1;
            }
        } else if let Flow::Remote { position, raw_event, .. } = &self.ctx.flow {
            let replaced_event_id = replacement.event_id.clone();
            let replacement = PendingEdit {
                kind: PendingEditKind::RoomMessage(replacement),
                event_json: raw_event.clone(),
            };
            self.stash_pending_edit(*position, replaced_event_id, replacement);
        } else {
            debug!("Local message edit for a timeline item not found, discarding");
        }
    }

    /// Try to stash a pending edit, if it makes sense to do so.
    #[instrument(skip(self, replacement))]
    fn stash_pending_edit(
        &mut self,
        position: TimelineItemPosition,
        replaced_event_id: OwnedEventId,
        replacement: PendingEdit,
    ) {
        match position {
            TimelineItemPosition::Start { .. } | TimelineItemPosition::Update(_) => {
                // Only insert the edit if there wasn't any other edit
                // before.
                //
                // For a start position, this is the right thing to do, because if there was a
                // stashed edit, it relates to a more recent one (either appended for a live
                // sync, or inserted earlier via back-pagination).
                //
                // For an update position, if there was a stashed edit, we can't really know
                // which version is the more recent, without an ordering of the
                // edit events themselves, so we discard it in that case.
                if !self
                    .meta
                    .pending_edits
                    .iter()
                    .any(|edit| edit.edited_event() == replaced_event_id)
                {
                    self.meta.pending_edits.push(replacement);
                    debug!("Timeline item not found, stashing edit");
                } else {
                    debug!("Timeline item not found, but there was a previous edit for the event: discarding");
                }
            }

            TimelineItemPosition::End { .. } => {
                // This is a more recent edit, coming either live from sync or from a
                // forward-pagination: it's fine to overwrite the previous one, if
                // available.
                let edits = &mut self.meta.pending_edits;
                let _ = Self::maybe_unstash_pending_edit(edits, &replaced_event_id);
                edits.push(replacement);
                debug!("Timeline item not found, stashing edit");
            }
        }
    }

    /// Look for a pending edit for the given event, and remove it from the list
    /// and return it, if found.
    fn maybe_unstash_pending_edit(
        edits: &mut RingBuffer<PendingEdit>,
        event_id: &EventId,
    ) -> Option<PendingEdit> {
        let pos = edits.iter().position(|edit| edit.edited_event() == event_id)?;
        trace!(edited_event = %event_id, "unstashed pending edit");
        Some(edits.remove(pos).unwrap())
    }

    /// Try applying an edit to an existing [`EventTimelineItem`].
    ///
    /// Return a new item if applying the edit succeeded, or `None` if there was
    /// an error while applying it.
    fn apply_msg_edit(
        &self,
        item: &EventTimelineItem,
        new_content: RoomMessageEventContentWithoutRelation,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) -> Option<EventTimelineItem> {
        if self.ctx.sender != item.sender() {
            info!(
                original_sender = ?item.sender(), edit_sender = ?self.ctx.sender,
                "Edit event applies to another user's timeline item, discarding"
            );
            return None;
        }

        let TimelineItemContent::Message(msg) = item.content() else {
            info!(
                "Edit of message event applies to {:?}, discarding",
                item.content().debug_string(),
            );
            return None;
        };

        let mut new_msg = msg.clone();
        new_msg.apply_edit(new_content);

        let mut new_item = item.with_content(TimelineItemContent::Message(new_msg), edit_json);

        if let EventTimelineItemKind::Remote(remote_event) = &item.kind {
            if let Flow::Remote { encryption_info, .. } = &self.ctx.flow {
                new_item = new_item.with_kind(EventTimelineItemKind::Remote(
                    remote_event.with_encryption_info(encryption_info.clone()),
                ));
            }
        }

        Some(new_item)
    }

    // Redacted reaction events are no-ops so don't need to be handled
    #[instrument(skip_all, fields(relates_to_event_id = ?c.relates_to.event_id))]
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let reacted_to_event_id = &c.relates_to.event_id;

        let (reaction_id, send_handle, old_txn_id) = match &self.ctx.flow {
            Flow::Local { txn_id, send_handle, .. } => {
                (TimelineEventItemId::TransactionId(txn_id.clone()), send_handle.clone(), None)
            }
            Flow::Remote { event_id, txn_id, .. } => {
                (TimelineEventItemId::EventId(event_id.clone()), None, txn_id.as_ref())
            }
        };

        if let Some((idx, event_item)) = rfind_event_by_id(self.items, reacted_to_event_id) {
            // Ignore reactions on redacted events.
            if let TimelineItemContent::RedactedMessage = event_item.content() {
                debug!("Ignoring reaction on redacted event");
                return;
            }

            trace!("Added reaction");

            // Add the reaction to the event item's bundled reactions.
            let mut reactions = event_item.reactions.clone();

            reactions.entry(c.relates_to.key.clone()).or_default().insert(
                self.ctx.sender.clone(),
                ReactionInfo {
                    timestamp: self.ctx.timestamp,
                    status: match &reaction_id {
                        TimelineEventItemId::TransactionId(_txn_id) => {
                            ReactionStatus::LocalToRemote(send_handle)
                        }
                        TimelineEventItemId::EventId(event_id) => {
                            ReactionStatus::RemoteToRemote(event_id.clone())
                        }
                    },
                },
            );

            self.items.set(idx, event_item.with_reactions(reactions));

            self.result.items_updated += 1;
        } else {
            trace!("Timeline item not found, adding reaction to the pending list");

            let TimelineEventItemId::EventId(reaction_event_id) = reaction_id.clone() else {
                error!("Adding local reaction echo to event absent from the timeline");
                return;
            };

            self.meta.reactions.pending.entry(reacted_to_event_id.to_owned()).or_default().insert(
                reaction_event_id,
                PendingReaction {
                    key: c.relates_to.key.clone(),
                    sender_id: self.ctx.sender.clone(),
                    timestamp: self.ctx.timestamp,
                },
            );
        }

        if let Some(txn_id) = old_txn_id {
            // Try to remove a local echo of that reaction. It might be missing if the
            // reaction wasn't sent by this device, or was sent in a previous
            // session.
            self.meta.reactions.map.remove(&TimelineEventItemId::TransactionId(txn_id.clone()));
        }

        self.meta.reactions.map.insert(
            reaction_id,
            FullReactionKey {
                item: TimelineEventItemId::EventId(c.relates_to.event_id),
                sender: self.ctx.sender.clone(),
                key: c.relates_to.key,
            },
        );
    }

    #[instrument(skip_all, fields(replacement_event_id = ?replacement.event_id))]
    fn handle_poll_edit(
        &mut self,
        replacement: Replacement<NewUnstablePollStartEventContentWithoutRelation>,
    ) {
        let Some((item_pos, item)) = rfind_event_by_id(self.items, &replacement.event_id) else {
            if let Flow::Remote { position, raw_event, .. } = &self.ctx.flow {
                let replaced_event_id = replacement.event_id.clone();
                let replacement = PendingEdit {
                    kind: PendingEditKind::Poll(replacement),
                    event_json: raw_event.clone(),
                };
                self.stash_pending_edit(*position, replaced_event_id, replacement);
            } else {
                debug!("Local poll edit for a timeline item not found, discarding");
            }
            return;
        };

        let edit_json = self.ctx.flow.raw_event().cloned();

        let Some(new_item) = self.apply_poll_edit(item.inner, replacement, edit_json) else {
            return;
        };

        trace!("Applying poll start edit.");
        self.items.set(item_pos, TimelineItem::new(new_item, item.internal_id.to_owned()));
        self.result.items_updated += 1;
    }

    fn apply_poll_edit(
        &self,
        item: &EventTimelineItem,
        replacement: Replacement<NewUnstablePollStartEventContentWithoutRelation>,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) -> Option<EventTimelineItem> {
        if self.ctx.sender != item.sender() {
            info!(
                original_sender = ?item.sender(), edit_sender = ?self.ctx.sender,
                "Edit event applies to another user's timeline item, discarding"
            );
            return None;
        }

        let TimelineItemContent::Poll(poll_state) = &item.content() else {
            info!("Edit of poll event applies to {}, discarding", item.content().debug_string(),);
            return None;
        };

        let new_content = match poll_state.edit(replacement.new_content) {
            Some(edited_poll_state) => TimelineItemContent::Poll(edited_poll_state),
            None => {
                info!("Not applying edit to a poll that's already ended");
                return None;
            }
        };

        Some(item.with_content(new_content, edit_json))
    }

    /// Adds a new poll to the timeline.
    fn handle_poll_start(
        &mut self,
        c: NewUnstablePollStartEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
    ) {
        // Always remove the pending edit, if there's any. The reason is that if
        // there's an edit in the relations mapping, we want to prefer it over any
        // other pending edit, since it's more likely to be up to date, and we
        // don't want to apply another pending edit on top of it.
        let pending_edit = self
            .ctx
            .flow
            .event_id()
            .and_then(|event_id| {
                Self::maybe_unstash_pending_edit(&mut self.meta.pending_edits, event_id)
            })
            .and_then(|edit| match edit.kind {
                PendingEditKind::Poll(replacement) => {
                    Some((Some(edit.event_json), replacement.new_content))
                }
                _ => None,
            });

        let (edit_json, edit_content) = extract_poll_edit_content(relations)
            .map(|content| {
                let edit_json = self.ctx.flow.raw_event().and_then(extract_bundled_edit_event_json);
                (edit_json, content)
            })
            .or(pending_edit)
            .unzip();

        let mut poll_state = PollState::new(c, edit_content);

        if let Some(event_id) = self.ctx.flow.event_id() {
            // Applying the cache to remote events only because local echoes
            // don't have an event ID that could be referenced by responses yet.
            self.meta.pending_poll_events.apply_pending(event_id, &mut poll_state);
        }

        let edit_json = edit_json.flatten();

        self.add_item(TimelineItemContent::Poll(poll_state), edit_json);
    }

    fn handle_poll_response(&mut self, c: UnstablePollResponseEventContent) {
        let Some((item_pos, item)) = rfind_event_by_id(self.items, &c.relates_to.event_id) else {
            self.meta.pending_poll_events.add_response(
                &c.relates_to.event_id,
                &self.ctx.sender,
                self.ctx.timestamp,
                &c,
            );
            return;
        };

        let TimelineItemContent::Poll(poll_state) = item.content() else {
            return;
        };

        let new_item = item.with_content(
            TimelineItemContent::Poll(poll_state.add_response(
                &self.ctx.sender,
                self.ctx.timestamp,
                &c,
            )),
            None,
        );

        trace!("Adding poll response.");
        self.items.set(item_pos, TimelineItem::new(new_item, item.internal_id.to_owned()));
        self.result.items_updated += 1;
    }

    fn handle_poll_end(&mut self, c: UnstablePollEndEventContent) {
        let Some((item_pos, item)) = rfind_event_by_id(self.items, &c.relates_to.event_id) else {
            self.meta.pending_poll_events.mark_as_ended(&c.relates_to.event_id, self.ctx.timestamp);
            return;
        };

        let TimelineItemContent::Poll(poll_state) = item.content() else {
            return;
        };

        match poll_state.end(self.ctx.timestamp) {
            Ok(poll_state) => {
                let new_item = item.with_content(TimelineItemContent::Poll(poll_state), None);

                trace!("Ending poll.");
                self.items.set(item_pos, TimelineItem::new(new_item, item.internal_id.to_owned()));
                self.result.items_updated += 1;
            }
            Err(_) => {
                info!("Got multiple poll end events, discarding");
            }
        }
    }

    /// Looks for the redacted event in all the timeline event items, and
    /// redacts it.
    ///
    /// This only applies to *remote* events; for local items being redacted,
    /// use [`Self::handle_reaction_redaction`].
    ///
    /// This assumes the redacted event was present in the timeline in the first
    /// place; it will warn if the redacted event has not been found.
    #[instrument(skip_all, fields(redacts_event_id = ?redacted))]
    fn handle_redaction(&mut self, redacted: OwnedEventId) {
        // TODO: Apply local redaction of PollResponse and PollEnd events.
        // https://github.com/matrix-org/matrix-rust-sdk/pull/2381#issuecomment-1689647825

        // If it's a reaction that's being redacted, handle it here.
        if self.handle_reaction_redaction(TimelineEventItemId::EventId(redacted.clone())) {
            // When we have raw timeline items, we should not return here anymore, as we
            // might need to redact the raw item as well.
            return;
        }

        // General path: redact another kind of (non-reaction) event.
        if let Some((idx, item)) = rfind_event_by_id(self.items, &redacted) {
            if item.as_remote().is_some() {
                if let TimelineItemContent::RedactedMessage = &item.content {
                    debug!("event item is already redacted");
                } else {
                    let new_item = item.redact(&self.meta.room_version);
                    self.items.set(idx, TimelineItem::new(new_item, item.internal_id.to_owned()));
                    self.result.items_updated += 1;
                }
            } else {
                error!("inconsistent state: redaction received on a non-remote event item");
            }
        } else {
            debug!("Timeline item not found, discarding redaction");
        };

        // Look for any timeline event that's a reply to the redacted event, and redact
        // the replied-to event there as well.
        self.items.for_each(|mut entry| {
            let Some(event_item) = entry.as_event() else { return };
            let Some(message) = event_item.content.as_message() else { return };
            let Some(in_reply_to) = message.in_reply_to() else { return };
            let TimelineDetails::Ready(replied_to_event) = &in_reply_to.event else { return };
            if redacted == in_reply_to.event_id {
                let replied_to_event = replied_to_event.redact(&self.meta.room_version);
                let in_reply_to = InReplyToDetails {
                    event_id: in_reply_to.event_id.clone(),
                    event: TimelineDetails::Ready(Box::new(replied_to_event)),
                };
                let content = TimelineItemContent::Message(message.with_in_reply_to(in_reply_to));
                let new_item = entry.with_kind(event_item.with_content(content, None));

                ObservableVectorTransactionEntry::set(&mut entry, new_item);
            }
        });
    }

    /// Attempts to redact a reaction, local or remote.
    ///
    /// Returns true if it's succeeded.
    #[instrument(skip_all, fields(redacts = ?reaction_id))]
    fn handle_reaction_redaction(&mut self, reaction_id: TimelineEventItemId) -> bool {
        if let Some(FullReactionKey {
            item: TimelineEventItemId::EventId(reacted_to_event_id),
            key,
            sender,
        }) = self.meta.reactions.map.remove(&reaction_id)
        {
            let Some((item_pos, item)) = rfind_event_by_id(self.items, &reacted_to_event_id) else {
                // The remote event wasn't in the timeline.
                if let TimelineEventItemId::EventId(event_id) = reaction_id {
                    // Remove any possibly pending reactions to that event, as this redaction would
                    // affect them.
                    if let Some(reactions) =
                        self.meta.reactions.pending.get_mut(&reacted_to_event_id)
                    {
                        reactions.swap_remove(&event_id);
                    }
                }

                // We haven't redacted the reaction.
                return false;
            };

            let mut reactions = item.reactions.clone();
            if reactions.remove_reaction(&sender, &key).is_some() {
                trace!("Removing reaction");
                self.items.set(item_pos, item.with_reactions(reactions));
                self.result.items_updated += 1;
                return true;
            }
        }

        false
    }

    /// Add a new event item in the timeline.
    fn add_item(
        &mut self,
        content: TimelineItemContent,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) {
        self.result.item_added = true;

        let sender = self.ctx.sender.to_owned();
        let sender_profile = TimelineDetails::from_initial_value(self.ctx.sender_profile.clone());
        let timestamp = self.ctx.timestamp;
        let reactions = self.pending_reactions(&content).unwrap_or_default();

        let kind: EventTimelineItemKind = match &self.ctx.flow {
            Flow::Local { txn_id, send_handle } => LocalEventTimelineItem {
                send_state: EventSendState::NotSentYet,
                transaction_id: txn_id.to_owned(),
                send_handle: send_handle.clone(),
            }
            .into(),

            Flow::Remote { event_id, raw_event, position, txn_id, encryption_info, .. } => {
                let origin = match *position {
                    TimelineItemPosition::Start { origin }
                    | TimelineItemPosition::End { origin } => origin,

                    // For updates, reuse the origin of the encrypted event.
                    TimelineItemPosition::Update(idx) => self.items[idx]
                        .as_event()
                        .and_then(|ev| Some(ev.as_remote()?.origin))
                        .unwrap_or_else(|| {
                            error!("Decryption retried on a local event");
                            RemoteEventOrigin::Unknown
                        }),
                };

                RemoteEventTimelineItem {
                    event_id: event_id.clone(),
                    transaction_id: txn_id.clone(),
                    read_receipts: self.ctx.read_receipts.clone(),
                    is_own: self.ctx.is_own_event,
                    is_highlighted: self.ctx.is_highlighted,
                    encryption_info: encryption_info.clone(),
                    original_json: Some(raw_event.clone()),
                    latest_edit_json: edit_json,
                    origin,
                }
                .into()
            }
        };

        let is_room_encrypted = if let Ok(is_room_encrypted) = self.meta.is_room_encrypted.read() {
            is_room_encrypted.unwrap_or_default()
        } else {
            false
        };

        let mut item = EventTimelineItem::new(
            sender,
            sender_profile,
            timestamp,
            content,
            kind,
            reactions,
            is_room_encrypted,
        );

        match &self.ctx.flow {
            Flow::Local { .. } => {
                trace!("Adding new local timeline item");

                let item = self.meta.new_timeline_item(item);
                self.items.push_back(item);
            }

            Flow::Remote { position: TimelineItemPosition::Start { .. }, event_id, .. } => {
                if self
                    .items
                    .iter()
                    .filter_map(|ev| ev.as_event()?.event_id())
                    .any(|id| id == event_id)
                {
                    trace!("Skipping back-paginated event that has already been seen");
                    return;
                }

                trace!("Adding new remote timeline item at the start");

                let item = self.meta.new_timeline_item(item);
                self.items.push_front(item);
            }

            Flow::Remote {
                position: TimelineItemPosition::End { .. }, txn_id, event_id, ..
            } => {
                // Look if we already have a corresponding item somewhere, based on the
                // transaction id (if a local echo) or the event id (if a
                // duplicate remote event).
                let result = rfind_event_item(self.items, |it| {
                    txn_id.is_some() && it.transaction_id() == txn_id.as_deref()
                        || it.event_id() == Some(event_id)
                });

                let mut removed_event_item_id = None;

                if let Some((idx, old_item)) = result {
                    if old_item.as_remote().is_some() {
                        // Item was previously received from the server. This should be very rare
                        // normally, but with the sliding- sync proxy, it is actually very
                        // common.
                        // NOTE: SS proxy workaround.
                        trace!(?item, old_item = ?*old_item, "Received duplicate event");

                        if old_item.content.is_redacted() && !item.content.is_redacted() {
                            warn!("Got original form of an event that was previously redacted");
                            item.content = item.content.redact(&self.meta.room_version);
                            item.reactions.clear();
                        }
                    }

                    // TODO: Check whether anything is different about the
                    //       old and new item?

                    transfer_details(&mut item, &old_item);

                    let old_item_id = old_item.internal_id;

                    if idx == self.items.len() - 1 {
                        // If the old item is the last one and no day divider
                        // changes need to happen, replace and return early.
                        trace!(idx, "Replacing existing event");
                        self.items.set(idx, TimelineItem::new(item, old_item_id.to_owned()));
                        return;
                    }

                    // In more complex cases, remove the item before re-adding the item.
                    trace!("Removing local echo or duplicate timeline item");
                    removed_event_item_id = Some(self.items.remove(idx).internal_id.clone());

                    // no return here, below code for adding a new event
                    // will run to re-add the removed item
                }

                // Local echoes that are pending should stick to the bottom,
                // find the latest event that isn't that.
                let latest_event_idx = self
                    .items
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, item)| (!item.as_event()?.is_local_echo()).then_some(idx));

                // Insert the next item after the latest event item that's not a
                // pending local echo, or at the start if there is no such item.
                let insert_idx = latest_event_idx.map_or(0, |idx| idx + 1);

                trace!("Adding new remote timeline item after all non-pending events");
                let new_item = match removed_event_item_id {
                    // If a previous version of the same item (usually a local
                    // echo) was removed and we now need to add it again, reuse
                    // the previous item's ID.
                    Some(id) => TimelineItem::new(item, id),
                    None => self.meta.new_timeline_item(item),
                };

                // Keep push semantics, if we're inserting at the front or the back.
                if insert_idx == self.items.len() {
                    self.items.push_back(new_item);
                } else if insert_idx == 0 {
                    self.items.push_front(new_item);
                } else {
                    self.items.insert(insert_idx, new_item);
                }
            }

            Flow::Remote { position: TimelineItemPosition::Update(idx), .. } => {
                trace!("Updating timeline item at position {idx}");
                let internal_id = self.items[*idx].internal_id.clone();
                self.items.set(*idx, TimelineItem::new(item, internal_id));
            }
        }

        // If we don't have a read marker item, look if we need to add one now.
        if !self.meta.has_up_to_date_read_marker_item {
            self.meta.update_read_marker(self.items);
        }
    }

    fn pending_reactions(
        &mut self,
        content: &TimelineItemContent,
    ) -> Option<ReactionsByKeyBySender> {
        // Drop pending reactions if the message is redacted.
        if let TimelineItemContent::RedactedMessage = content {
            return None;
        }

        self.ctx.flow.event_id().and_then(|event_id| {
            let reactions = self.meta.reactions.pending.remove(event_id)?;
            let mut bundled = ReactionsByKeyBySender::default();

            for (reaction_event_id, reaction) in reactions {
                let group: &mut IndexMap<OwnedUserId, ReactionInfo> =
                    bundled.entry(reaction.key).or_default();

                group.insert(
                    reaction.sender_id,
                    ReactionInfo {
                        timestamp: reaction.timestamp,
                        status: ReactionStatus::RemoteToRemote(reaction_event_id),
                    },
                );
            }

            Some(bundled)
        })
    }
}

/// Transfer `TimelineDetails` that weren't available on the original item and
/// have been fetched separately (only `reply_to` for now) from `old_item` to
/// `item`, given two items for an event that was re-received.
///
/// `old_item` *should* always be a local echo usually, but with the sliding
/// sync proxy, we often re-receive remote events that aren't remote echoes.
fn transfer_details(item: &mut EventTimelineItem, old_item: &EventTimelineItem) {
    let TimelineItemContent::Message(msg) = &mut item.content else { return };
    let TimelineItemContent::Message(old_msg) = &old_item.content else { return };

    let Some(in_reply_to) = &mut msg.in_reply_to else { return };
    let Some(old_in_reply_to) = &old_msg.in_reply_to else { return };

    if matches!(&in_reply_to.event, TimelineDetails::Unavailable) {
        in_reply_to.event = old_in_reply_to.event.clone();
    }
}
