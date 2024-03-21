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

use std::{fmt::Display, sync::Arc};

use as_variant::as_variant;
use eyeball_im::{ObservableVectorTransaction, ObservableVectorTransactionEntry};
use indexmap::{map::Entry, IndexMap};
use matrix_sdk::deserialized_responses::EncryptionInfo;
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
            redaction::{RoomRedactionEventContent, SyncRoomRedactionEvent},
        },
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, BundledMessageLikeRelations, EventContent, FullStateEventContent,
        MessageLikeEventType, StateEventType, SyncStateEvent,
    },
    html::RemoveReplyFallback,
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId,
};
use tracing::{debug, error, event_enabled, field::debug, info, instrument, trace, warn, Level};

use super::{
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EventItemIdentifier, EventSendState,
        EventTimelineItemKind, LocalEventTimelineItem, Profile, RemoteEventOrigin,
        RemoteEventTimelineItem,
    },
    inner::{TimelineInnerMetadata, TimelineInnerStateTransaction},
    polls::PollState,
    util::{rfind_event_by_id, rfind_event_item, timestamp_to_date},
    EventTimelineItem, InReplyToDetails, Message, OtherState, ReactionGroup, ReactionSenderData,
    Sticker, TimelineDetails, TimelineItem, TimelineItemContent, TimelineItemKind,
    VirtualTimelineItem,
};
use crate::{events::SyncTimelineEventWithoutContent, DEFAULT_SANITIZER_MODE};

/// When adding an event, useful information related to the source of the event.
#[derive(Clone)]
pub(super) enum Flow {
    /// The event was locally created.
    Local {
        /// The transaction id we've used in requests associated to this event.
        txn_id: OwnedTransactionId,
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
        /// Should this event actually be added, based on the event filters.
        should_add: bool,
    },
}

#[derive(Clone)]
pub(super) struct TimelineEventContext {
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: Option<Profile>,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) is_own_event: bool,
    pub(super) encryption_info: Option<EncryptionInfo>,
    pub(super) read_receipts: IndexMap<OwnedUserId, Receipt>,
    pub(super) is_highlighted: bool,
    pub(super) flow: Flow,
}

#[derive(Clone, Debug)]
pub(super) enum TimelineEventKind {
    Message {
        content: AnyMessageLikeEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
    },
    RedactedMessage {
        event_type: MessageLikeEventType,
    },
    Redaction {
        redacts: OwnedEventId,
        content: RoomRedactionEventContent,
    },
    LocalRedaction {
        redacts: OwnedTransactionId,
        content: RoomRedactionEventContent,
    },
    RoomMember {
        user_id: OwnedUserId,
        content: FullStateEventContent<RoomMemberEventContent>,
        sender: OwnedUserId,
    },
    OtherState {
        state_key: String,
        content: AnyOtherFullStateEventContent,
    },
    FailedToParseMessageLike {
        event_type: MessageLikeEventType,
        error: Arc<serde_json::Error>,
    },
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
                    let content = match ev {
                        SyncRoomRedactionEvent::Original(e) => e.content,
                        SyncRoomRedactionEvent::Redacted(_) => Default::default(),
                    };

                    Self::Redaction { redacts, content }
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
    ///
    /// Usually this means items coming from back-pagination.
    Start,

    /// One or more items are appended to the timeline (i.e. they're the most
    /// recent).
    End {
        /// Whether this event is coming from a local cache.
        from_cache: bool,
    },

    /// A single item is updated.
    ///
    /// This only happens when a UTD must be replaced with the decrypted event.
    #[cfg(feature = "e2e-encryption")]
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
    #[cfg(feature = "e2e-encryption")]
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
    meta: &'a mut TimelineInnerMetadata,
    ctx: TimelineEventContext,
    result: HandleEventResult,
}

impl<'a, 'o> TimelineEventHandler<'a, 'o> {
    pub(super) fn new(
        state: &'a mut TimelineInnerStateTransaction<'o>,
        ctx: TimelineEventContext,
    ) -> Self {
        let TimelineInnerStateTransaction { items, meta, .. } = state;
        Self { items, meta, ctx, result: HandleEventResult::default() }
    }

    /// Handle an event.
    ///
    /// Returns the number of timeline updates that were made.
    #[instrument(skip_all, fields(txn_id, event_id, position))]
    pub(super) fn handle_event(mut self, event_kind: TimelineEventKind) -> HandleEventResult {
        let span = tracing::Span::current();

        let should_add = match &self.ctx.flow {
            Flow::Local { txn_id, .. } => {
                span.record("txn_id", debug(txn_id));
                debug!("Handling local event");

                true
            }

            Flow::Remote { event_id, txn_id, position, should_add, .. } => {
                span.record("event_id", debug(event_id));
                span.record("position", debug(position));
                if let Some(txn_id) = txn_id {
                    span.record("txn_id", debug(txn_id));
                }
                trace!("Handling remote event");

                *should_add
            }
        };

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
                    self.add(should_add, TimelineItemContent::message(c, relations, self.items));
                }
                AnyMessageLikeEventContent::RoomEncrypted(c) => {
                    // TODO: Handle replacements if the replaced event is also UTD
                    self.add(true, TimelineItemContent::unable_to_decrypt(c));

                    // Let the hook know that we ran into an unable-to-decrypt that is added to the
                    // timeline.
                    if let Some(hook) = self.meta.unable_to_decrypt_hook.as_ref() {
                        if let Flow::Remote { event_id, .. } = &self.ctx.flow {
                            hook.on_utd(event_id);
                        }
                    }
                }
                AnyMessageLikeEventContent::Sticker(content) => {
                    self.add(should_add, TimelineItemContent::Sticker(Sticker { content }));
                }
                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::Replacement(c),
                ) => self.handle_poll_start_edit(c.relates_to),
                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::New(c),
                ) => self.handle_poll_start(c, should_add),
                AnyMessageLikeEventContent::UnstablePollResponse(c) => self.handle_poll_response(c),
                AnyMessageLikeEventContent::UnstablePollEnd(c) => self.handle_poll_end(c),
                AnyMessageLikeEventContent::CallInvite(_) => {
                    self.add(should_add, TimelineItemContent::CallInvite);
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
                if event_type != MessageLikeEventType::Reaction {
                    self.add(should_add, TimelineItemContent::RedactedMessage);
                }
            }

            TimelineEventKind::Redaction { redacts, content } => {
                self.handle_redaction(redacts, content);
            }
            TimelineEventKind::LocalRedaction { redacts, content } => {
                self.handle_local_redaction(redacts, content);
            }

            TimelineEventKind::RoomMember { user_id, content, sender } => {
                self.add(should_add, TimelineItemContent::room_member(user_id, content, sender));
            }

            TimelineEventKind::OtherState { state_key, content } => {
                self.add(
                    should_add,
                    TimelineItemContent::OtherState(OtherState { state_key, content }),
                );
            }

            TimelineEventKind::FailedToParseMessageLike { event_type, error } => {
                self.add(
                    should_add,
                    TimelineItemContent::FailedToParseMessageLike { event_type, error },
                );
            }

            TimelineEventKind::FailedToParseState { event_type, state_key, error } => {
                self.add(
                    should_add,
                    TimelineItemContent::FailedToParseState { event_type, state_key, error },
                );
            }
        }

        if !self.result.item_added {
            trace!("No new item added");

            #[cfg(feature = "e2e-encryption")]
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

    #[instrument(skip_all, fields(replacement_event_id = ?replacement.event_id))]
    fn handle_room_message_edit(
        &mut self,
        replacement: Replacement<RoomMessageEventContentWithoutRelation>,
    ) {
        let found = self.update_timeline_item(&replacement.event_id, |this, event_item| {
            if this.ctx.sender != event_item.sender() {
                info!(
                    original_sender = ?event_item.sender(), edit_sender = ?this.ctx.sender,
                    "Edit event applies to another user's timeline item, discarding"
                );
                return None;
            }

            let TimelineItemContent::Message(msg) = event_item.content() else {
                info!(
                    "Edit of message event applies to {}, discarding",
                    event_item.content().debug_string(),
                );
                return None;
            };

            let mut msgtype = replacement.new_content.msgtype;
            // Edit's content is never supposed to contain the reply fallback.
            msgtype.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::No);

            let new_content = TimelineItemContent::Message(Message {
                msgtype,
                in_reply_to: msg.in_reply_to.clone(),
                thread_root: msg.thread_root.clone(),
                edited: true,
            });

            let edit_json = match &this.ctx.flow {
                Flow::Local { .. } => None,
                Flow::Remote { raw_event, .. } => Some(raw_event.clone()),
            };

            trace!("Applying edit");
            Some(event_item.with_content(new_content, edit_json))
        });

        if !found {
            debug!("Timeline item not found, discarding edit");
        }
    }

    // Redacted reaction events are no-ops so don't need to be handled
    #[instrument(skip_all, fields(relates_to_event_id = ?c.relates_to.event_id))]
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let event_id: &EventId = &c.relates_to.event_id;
        let (reaction_id, old_txn_id) = match &self.ctx.flow {
            Flow::Local { txn_id, .. } => {
                (EventItemIdentifier::TransactionId(txn_id.clone()), None)
            }
            Flow::Remote { event_id, txn_id, .. } => {
                (EventItemIdentifier::EventId(event_id.clone()), txn_id.as_ref())
            }
        };

        if let Some((idx, event_item)) = rfind_event_by_id(self.items, event_id) {
            let Some(remote_event_item) = event_item.as_remote() else {
                error!("inconsistent state: reaction received on a non-remote event item");
                return;
            };

            // Handling of reactions on redacted events is an open question.
            // For now, ignore reactions on redacted events like Element does.
            if let TimelineItemContent::RedactedMessage = event_item.content() {
                debug!("Ignoring reaction on redacted event");
                return;
            } else {
                let mut reactions = remote_event_item.reactions.clone();
                let reaction_group = reactions.entry(c.relates_to.key.clone()).or_default();

                if let Some(txn_id) = old_txn_id {
                    let id = EventItemIdentifier::TransactionId(txn_id.clone());
                    // Remove the local echo from the related event.
                    if reaction_group.0.swap_remove(&id).is_none() {
                        warn!(
                            "Received reaction with transaction ID, but didn't \
                             find matching reaction in the related event's reactions"
                        );
                    }
                }
                reaction_group.0.insert(
                    reaction_id.clone(),
                    ReactionSenderData {
                        sender_id: self.ctx.sender.clone(),
                        timestamp: self.ctx.timestamp,
                    },
                );

                trace!("Adding reaction");
                self.items.set(
                    idx,
                    event_item.with_inner_kind(remote_event_item.with_reactions(reactions)),
                );
                self.result.items_updated += 1;
            }
        } else {
            trace!("Timeline item not found, adding reaction to the pending list");
            let EventItemIdentifier::EventId(reaction_event_id) = reaction_id.clone() else {
                error!("Adding local reaction echo to event absent from the timeline");
                return;
            };

            let pending = self.meta.reactions.pending.entry(event_id.to_owned()).or_default();

            pending.insert(reaction_event_id);
        }

        if let Flow::Remote { txn_id: Some(txn_id), .. } = &self.ctx.flow {
            let id = EventItemIdentifier::TransactionId(txn_id.clone());
            // Remove the local echo from the reaction map.
            if self.meta.reactions.map.remove(&id).is_none() {
                warn!(
                    "Received reaction with transaction ID, but didn't \
                     find matching reaction in reaction_map"
                );
            }
        }
        let reaction_sender_data = ReactionSenderData {
            sender_id: self.ctx.sender.clone(),
            timestamp: self.ctx.timestamp,
        };
        self.meta.reactions.map.insert(reaction_id, (reaction_sender_data, c.relates_to));
    }

    #[instrument(skip_all, fields(replacement_event_id = ?replacement.event_id))]
    fn handle_poll_start_edit(
        &mut self,
        replacement: Replacement<NewUnstablePollStartEventContentWithoutRelation>,
    ) {
        let found = self.update_timeline_item(&replacement.event_id, |this, event_item| {
            if this.ctx.sender != event_item.sender() {
                info!(
                    original_sender = ?event_item.sender(), edit_sender = ?this.ctx.sender,
                    "Edit event applies to another user's timeline item, discarding"
                );
                return None;
            }

            let TimelineItemContent::Poll(poll_state) = &event_item.content() else {
                info!(
                    "Edit of poll event applies to {}, discarding",
                    event_item.content().debug_string(),
                );
                return None;
            };

            let new_content = match poll_state.edit(&replacement.new_content) {
                Ok(edited_poll_state) => TimelineItemContent::Poll(edited_poll_state),
                Err(e) => {
                    info!("Failed to apply poll edit: {e:?}");
                    return None;
                }
            };

            let edit_json = match &this.ctx.flow {
                Flow::Local { .. } => None,
                Flow::Remote { raw_event, .. } => Some(raw_event.clone()),
            };

            trace!("Applying edit");
            Some(event_item.with_content(new_content, edit_json))
        });

        if !found {
            debug!("Timeline item not found, discarding poll edit");
        }
    }

    fn handle_poll_start(&mut self, c: NewUnstablePollStartEventContent, should_add: bool) {
        let mut poll_state = PollState::new(c);
        if let Flow::Remote { event_id, .. } = self.ctx.flow.clone() {
            // Applying the cache to remote events only because local echoes
            // don't have an event ID that could be referenced by responses yet.
            self.meta.poll_pending_events.apply(&event_id, &mut poll_state);
        }
        self.add(should_add, TimelineItemContent::Poll(poll_state));
    }

    fn handle_poll_response(&mut self, c: UnstablePollResponseEventContent) {
        let found = self.update_timeline_item(&c.relates_to.event_id, |this, event_item| {
            let poll_state = as_variant!(event_item.content(), TimelineItemContent::Poll)?;
            Some(event_item.with_content(
                TimelineItemContent::Poll(poll_state.add_response(
                    &this.ctx.sender,
                    this.ctx.timestamp,
                    &c,
                )),
                None,
            ))
        });

        if !found {
            self.meta.poll_pending_events.add_response(
                &c.relates_to.event_id,
                &self.ctx.sender,
                self.ctx.timestamp,
                &c,
            );
        }
    }

    fn handle_poll_end(&mut self, c: UnstablePollEndEventContent) {
        let found = self.update_timeline_item(&c.relates_to.event_id, |this, event_item| {
            let poll_state = as_variant!(event_item.content(), TimelineItemContent::Poll)?;
            match poll_state.end(this.ctx.timestamp) {
                Ok(poll_state) => {
                    Some(event_item.with_content(TimelineItemContent::Poll(poll_state), None))
                }
                Err(_) => {
                    info!("Got multiple poll end events, discarding");
                    None
                }
            }
        });

        if !found {
            self.meta.poll_pending_events.add_end(&c.relates_to.event_id, self.ctx.timestamp);
        }
    }

    // Redacted redactions are no-ops (unfortunately)
    #[instrument(skip_all, fields(redacts_event_id = ?redacts))]
    fn handle_redaction(&mut self, redacts: OwnedEventId, _content: RoomRedactionEventContent) {
        // TODO: Apply local redaction of PollResponse and PollEnd events.
        // https://github.com/matrix-org/matrix-rust-sdk/pull/2381#issuecomment-1689647825

        let id = EventItemIdentifier::EventId(redacts.clone());

        // Redact the reaction, if any.
        if let Some((_, rel)) = self.meta.reactions.map.remove(&id) {
            let found_reacted_to = self.update_timeline_item(&rel.event_id, |_this, event_item| {
                let Some(remote_event_item) = event_item.as_remote() else {
                    error!("inconsistent state: redaction received on a non-remote event item");
                    return None;
                };

                let mut reactions = remote_event_item.reactions.clone();

                let count = {
                    let Entry::Occupied(mut group_entry) = reactions.entry(rel.key.clone()) else {
                        return None;
                    };
                    let group = group_entry.get_mut();

                    if group.0.swap_remove(&id).is_none() {
                        error!(
                            "inconsistent state: reaction from reaction_map not in reaction list \
                             of timeline item"
                        );
                        return None;
                    }

                    group.len()
                };

                if count == 0 {
                    reactions.swap_remove(&rel.key);
                }

                trace!("Removing reaction");
                Some(event_item.with_kind(remote_event_item.with_reactions(reactions)))
            });

            if !found_reacted_to {
                debug!("Timeline item not found, discarding reaction redaction");
            }

            if self.result.items_updated == 0 {
                if let Some(reactions) = self.meta.reactions.pending.get_mut(&rel.event_id) {
                    if !reactions.swap_remove(&redacts.clone()) {
                        error!(
                            "inconsistent state: reaction from reaction_map not in reaction list \
                             of pending_reactions"
                        );
                    }
                } else {
                    warn!("reaction_map out of sync with timeline items");
                }
            }

            // Even if the event being redacted is a reaction (found in
            // `reaction_map`), it can still be present in the timeline items
            // directly with the raw event timeline feature (not yet
            // implemented) => no early return here.
        }

        let found_redacted_event = self.update_timeline_item(&redacts, |this, event_item| {
            if event_item.as_remote().is_none() {
                error!("inconsistent state: redaction received on a non-remote event item");
                return None;
            }

            if let TimelineItemContent::RedactedMessage = &event_item.content {
                debug!("event item is already redacted");
                return None;
            }

            Some(event_item.redact(&this.meta.room_version))
        });

        if !found_redacted_event {
            debug!("Timeline item not found, discarding redaction");
        }

        if self.result.items_updated == 0 {
            // We will want to know this when debugging redaction issues.
            debug!("redaction affected no event");
        }

        self.items.for_each(|mut entry| {
            let Some(event_item) = entry.as_event() else { return };
            let Some(message) = event_item.content.as_message() else { return };
            let Some(in_reply_to) = message.in_reply_to() else { return };
            let TimelineDetails::Ready(replied_to_event) = &in_reply_to.event else { return };
            if redacts == in_reply_to.event_id {
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

    // Redacted redactions are no-ops (unfortunately)
    #[instrument(skip_all, fields(redacts_event_id = ?redacts))]
    fn handle_local_redaction(
        &mut self,
        redacts: OwnedTransactionId,
        _content: RoomRedactionEventContent,
    ) {
        let id = EventItemIdentifier::TransactionId(redacts);

        // Redact the reaction, if any.
        if let Some((_, rel)) = self.meta.reactions.map.remove(&id) {
            let found = self.update_timeline_item(&rel.event_id, |_this, event_item| {
                let Some(remote_event_item) = event_item.as_remote() else {
                    error!("inconsistent state: redaction received on a non-remote event item");
                    return None;
                };

                let mut reactions = remote_event_item.reactions.clone();
                let Entry::Occupied(mut group_entry) = reactions.entry(rel.key.clone()) else {
                    return None;
                };
                let group = group_entry.get_mut();

                if group.0.swap_remove(&id).is_none() {
                    error!(
                        "inconsistent state: reaction from reaction_map not in reaction list \
                         of timeline item"
                    );
                    return None;
                }

                if group.len() == 0 {
                    group_entry.swap_remove();
                }

                trace!("Removing reaction");
                Some(event_item.with_kind(remote_event_item.with_reactions(reactions)))
            });

            if !found {
                debug!("Timeline item not found, discarding local redaction");
            }
        }

        if self.result.items_updated == 0 {
            // We will want to know this when debugging redaction issues.
            error!("redaction affected no event");
        }
    }

    /// Add a new event item in the timeline.
    fn add(&mut self, should_add: bool, content: TimelineItemContent) {
        if !should_add {
            return;
        }

        self.result.item_added = true;

        let sender = self.ctx.sender.to_owned();
        let sender_profile = TimelineDetails::from_initial_value(self.ctx.sender_profile.clone());
        let timestamp = self.ctx.timestamp;
        let mut reactions = self.pending_reactions().unwrap_or_default();

        let kind: EventTimelineItemKind = match &self.ctx.flow {
            Flow::Local { txn_id } => {
                LocalEventTimelineItem {
                    send_state: EventSendState::NotSentYet,
                    transaction_id: txn_id.to_owned(),
                }
            }
            .into(),

            Flow::Remote { event_id, raw_event, position, .. } => {
                // Drop pending reactions if the message is redacted.
                if let TimelineItemContent::RedactedMessage = content {
                    if !reactions.is_empty() {
                        reactions = BundledReactions::default();
                    }
                }

                let origin = match position {
                    TimelineItemPosition::Start => RemoteEventOrigin::Pagination,

                    // We only paginate backwards for now, so End only happens for syncs
                    TimelineItemPosition::End { from_cache: true } => RemoteEventOrigin::Cache,

                    TimelineItemPosition::End { from_cache: false } => RemoteEventOrigin::Sync,

                    #[cfg(feature = "e2e-encryption")]
                    TimelineItemPosition::Update(idx) => self.items[*idx]
                        .as_event()
                        .and_then(|ev| Some(ev.as_remote()?.origin))
                        .unwrap_or_else(|| {
                            error!("Decryption retried on a local event");
                            RemoteEventOrigin::Unknown
                        }),
                };

                RemoteEventTimelineItem {
                    event_id: event_id.clone(),
                    reactions,
                    read_receipts: self.ctx.read_receipts.clone(),
                    is_own: self.ctx.is_own_event,
                    is_highlighted: self.ctx.is_highlighted,
                    encryption_info: self.ctx.encryption_info.clone(),
                    original_json: Some(raw_event.clone()),
                    latest_edit_json: None,
                    origin,
                }
                .into()
            }
        };

        let mut item = EventTimelineItem::new(sender, sender_profile, timestamp, content, kind);

        match &self.ctx.flow {
            Flow::Local { .. } => {
                trace!("Adding new local timeline item");

                let item = self.meta.new_timeline_item(item);
                self.items.push_back(item);
            }

            Flow::Remote { position: TimelineItemPosition::Start, event_id, .. } => {
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
                            item.as_remote_mut()
                                .expect("Can't have a local item when flow == Remote")
                                .reactions
                                .clear();
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
                        self.items.set(idx, TimelineItem::new(item, old_item_id));
                        return;
                    }

                    // In more complex cases, remove the item before re-adding the item.
                    trace!("Removing local echo or duplicate timeline item");
                    removed_event_item_id = Some(self.items.remove(idx).internal_id);

                    // no return here, below code for adding a new event
                    // will run to re-add the removed item
                } else if txn_id.is_some() {
                    warn!(
                        "Received event with transaction ID, but didn't \
                         find matching timeline item"
                    );
                }

                // Local echoes that are pending should stick to the bottom,
                // find the latest event that isn't that.
                let latest_event_idx = self
                    .items
                    .iter()
                    .enumerate()
                    .rev()
                    .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
                    .find(|(_, item)| {
                        !matches!(
                            item.send_state(),
                            Some(EventSendState::NotSentYet | EventSendState::Sent { .. })
                        )
                    })
                    .unzip()
                    .0;

                // Insert the next item after the latest event item that's not a
                // pending local echo, or at the start if there is no such item.
                let insert_idx = latest_event_idx.map_or(0, |idx| idx + 1);

                let id = match removed_event_item_id {
                    // If a previous version of the same item (usually a local
                    // echo) was removed and we now need to add it again, reuse
                    // the previous item's ID.
                    Some(id) => id,
                    None => self.meta.next_internal_id(),
                };

                trace!("Adding new remote timeline item after all non-pending events");
                let new_item = TimelineItem::new(item, id);

                // Keep push semantics, if we're inserting at the front or the back.
                if insert_idx == self.items.len() {
                    self.items.push_back(new_item);
                } else if insert_idx == 0 {
                    self.items.push_front(new_item);
                } else {
                    self.items.insert(insert_idx, new_item);
                }
            }

            #[cfg(feature = "e2e-encryption")]
            Flow::Remote { position: TimelineItemPosition::Update(idx), .. } => {
                trace!("Updating timeline item at position {idx}");
                let id = self.items[*idx].internal_id;
                self.items.set(*idx, TimelineItem::new(item, id));
            }
        }

        // If we don't have a read marker item, look if we need to add one now.
        if !self.meta.has_up_to_date_read_marker_item {
            self.meta.update_read_marker(self.items);
        }
    }

    fn pending_reactions(&mut self) -> Option<BundledReactions> {
        match &self.ctx.flow {
            Flow::Local { .. } => None,
            Flow::Remote { event_id, .. } => {
                let reactions = self.meta.reactions.pending.remove(event_id)?;
                let mut bundled = IndexMap::new();

                for reaction_event_id in reactions {
                    let reaction_id = EventItemIdentifier::EventId(reaction_event_id);
                    let Some((reaction_sender_data, annotation)) =
                        self.meta.reactions.map.get(&reaction_id)
                    else {
                        error!(
                            "inconsistent state: reaction from pending_reactions not in reaction_map"
                        );
                        continue;
                    };

                    let group: &mut ReactionGroup =
                        bundled.entry(annotation.key.clone()).or_default();
                    group.0.insert(reaction_id, reaction_sender_data.clone());
                }

                Some(bundled)
            }
        }
    }

    /// Updates the given timeline item.
    ///
    /// Returns true iff the item has been found (not necessarily updated),
    /// false if it's not been found.
    fn update_timeline_item(
        &mut self,
        event_id: &EventId,
        update: impl FnOnce(&Self, &EventTimelineItem) -> Option<EventTimelineItem>,
    ) -> bool {
        if let Some((idx, item)) = rfind_event_by_id(self.items, event_id) {
            trace!("Found timeline item to update");
            if let Some(new_item) = update(self, item.inner) {
                trace!("Updating item");
                self.items.set(idx, TimelineItem::new(new_item, item.internal_id));
                self.result.items_updated += 1;
            }
            true
        } else {
            false
        }
    }
}

/// Algorithm ensuring that day dividers are adjusted correctly, according to
/// new items that have been inserted.
#[derive(Default)]
pub(super) struct DayDividerAdjuster {
    ops: Vec<DayDividerOperation>,
}

impl DayDividerAdjuster {
    /// Ensures that date separators are properly inserted/removed when needs
    /// be.
    #[instrument(skip(self))]
    pub fn maybe_adjust_day_dividers(
        mut self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        meta: &mut TimelineInnerMetadata,
    ) {
        // We're going to record vector operations like inserting, replacing and
        // removing day dividers. Since we may remove or insert new items,
        // recorded offsets will change as we're iterating over the array. The
        // only way this is possible is because we're recording operations
        // happening in non-decreasing order of the indices, i.e. we can't do an
        // operation onindex I and then on any index J<I later.
        //
        // Note we can't just iterate in reverse order, because we may have a
        // `Remove(i)` followed by a `Replace((i+1) -1)`, which wouldn't do what
        // we want, if running in reverse order.

        let mut prev_item: Option<&Arc<TimelineItem>> = None;
        let mut latest_event_ts = None;

        for (i, item) in items.iter().enumerate() {
            match item.kind() {
                TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) => {
                    self.handle_day_divider(i, *ts, prev_item);

                    prev_item = Some(item);
                }

                TimelineItemKind::Event(event) => {
                    let ts = event.timestamp();

                    self.handle_event(i, ts, prev_item, latest_event_ts);

                    prev_item = Some(item);
                    latest_event_ts = Some(ts);
                }

                TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                    // Nothing to do.
                }
            }
        }

        // Only record the initial state if we've enabled the trace log level, and not
        // otherwise.
        let initial_state =
            if event_enabled!(Level::TRACE) { Some(items.iter().cloned().collect()) } else { None };

        self.process_ops(items, meta);

        // Then check invariants.
        if let Some(report) = self.check_invariants(items, initial_state) {
            warn!("Errors encountered when checking invariants.");
            #[cfg(debug)]
            panic!("{report}");
            #[cfg(not(debug))]
            warn!("{report}");
        }
    }

    #[inline]
    fn handle_day_divider(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
    ) {
        let Some(prev_item) = prev_item else {
            // No interesting item prior to the day divider: it must be the first one,
            // nothing to do.
            return;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(event) => {
                // This day divider is preceded by an event.
                if is_same_date_as(event.timestamp(), ts) {
                    // The event has the same date as the day divider: remove the day
                    // divider.
                    trace!("removing day divider following event with same timestamp @ {i}");
                    self.ops.push(DayDividerOperation::Remove(i));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)) => {
                trace!("removing duplicate day divider @ {i}");
                // This day divider is preceded by another one: remove the current one.
                self.ops.push(DayDividerOperation::Remove(i));
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                // Nothing to do for read markers.
            }
        }
    }

    #[inline]
    fn handle_event(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
        latest_event_ts: Option<MilliSecondsSinceUnixEpoch>,
    ) {
        let Some(prev_item) = prev_item else {
            // The event was the first item, so there wasn't any day divider before it:
            // insert one.
            trace!("inserting the first day divider @ {}", i);
            self.ops.push(DayDividerOperation::Insert(i, ts));
            return;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(prev_event) => {
                // The event is preceded by another event. If they're not the same date,
                // insert a date divider.
                let prev_ts = prev_event.timestamp();

                if !is_same_date_as(prev_ts, ts) {
                    trace!("inserting day divider @ {} between two events with different dates", i);
                    self.ops.push(DayDividerOperation::Insert(i, ts));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(prev_ts)) => {
                let event_date = timestamp_to_date(ts);

                // The event is preceded by a day divider.
                if timestamp_to_date(*prev_ts) != event_date {
                    // The day divider is wrong. Should we replace it with the correct value, or
                    // remove it entirely?
                    let mut removed = false;

                    if let Some(last_event_ts) = latest_event_ts {
                        if timestamp_to_date(last_event_ts) == event_date {
                            // There's a previous event with the same date: remove the divider.
                            trace!("removed day divider @ {i} between two events that have the same date");
                            self.ops.push(DayDividerOperation::Remove(i - 1));
                            removed = true;
                        }
                    }

                    if !removed {
                        // There's no previous event or there's one with a different
                        // date: replace the current
                        // divider.
                        trace!("replacing day divider @ {i} with new timestamp from event");
                        self.ops.push(DayDividerOperation::Replace(i - 1, ts));
                    }
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                // Nothing to do.
            }
        }
    }

    fn process_ops(
        &self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        meta: &mut TimelineInnerMetadata,
    ) {
        // Record the deletion offset.
        let mut offset = 0i64;
        // Remember what the maximum index was, so we can assert that it's
        // non-decreasing.
        let mut max_i = 0;

        for op in &self.ops {
            match *op {
                DayDividerOperation::Insert(i, ts) => {
                    assert!(i >= max_i);

                    let at = (i64::try_from(i).unwrap() + offset)
                        .min(i64::try_from(items.len()).unwrap());
                    assert!(at >= 0);
                    let at = at as usize;

                    let item = meta.new_timeline_item(VirtualTimelineItem::DayDivider(ts));

                    // Keep push semantics, if we're inserting at the front or the back.
                    if at == items.len() {
                        items.push_back(item);
                    } else if at == 0 {
                        items.push_front(item);
                    } else {
                        items.insert(at, item);
                    }

                    offset += 1;
                    max_i = i;
                }

                DayDividerOperation::Replace(i, ts) => {
                    assert!(i >= max_i);

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);
                    let at = at as usize;

                    let replaced = &items[at];
                    assert!(replaced.is_day_divider(), "we replaced a non day-divider");

                    let unique_id = replaced.unique_id();
                    let item = TimelineItem::new(VirtualTimelineItem::DayDivider(ts), unique_id);

                    items.set(at, item);
                    max_i = i;
                }

                DayDividerOperation::Remove(i) => {
                    assert!(i >= max_i);

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);

                    let removed = items.remove(at as usize);
                    assert!(removed.is_day_divider(), "we removed a non day-divider");

                    offset -= 1;
                    max_i = i;
                }
            }
        }
    }

    /// Checks the invariants that must hold at any time after inserting day
    /// dividers.
    ///
    /// Returns a report if and only if there was at least one error.
    fn check_invariants<'a, 'o>(
        self,
        items: &'a ObservableVectorTransaction<'o, Arc<TimelineItem>>,
        initial_state: Option<Vec<Arc<TimelineItem>>>,
    ) -> Option<DayDividerInvariantsReport<'a, 'o>> {
        let mut report = DayDividerInvariantsReport {
            initial_state,
            errors: Vec::new(),
            operations: self.ops,
            final_state: items,
        };

        // Assert invariants.
        // 1. The timeline starts with a date separator.
        if let Some(item) = items.get(0) {
            if !item.is_day_divider() {
                report.errors.push(DayDividerInsertError::FirstItemNotDayDivider)
            }
        }

        // 2. There are no two date dividers following each other.
        {
            let mut prev_was_day_divider = false;
            for (i, item) in items.iter().enumerate() {
                if item.is_day_divider() {
                    if prev_was_day_divider {
                        report.errors.push(DayDividerInsertError::DuplicateDayDivider { at: i });
                    }
                    prev_was_day_divider = true;
                } else {
                    prev_was_day_divider = false;
                }
            }
        };

        // 3. There's no trailing day divider.
        if let Some(last) = items.last() {
            if last.is_day_divider() {
                report.errors.push(DayDividerInsertError::TrailingDayDivider);
            }
        }

        // 4. Items are properly separated with day dividers.
        {
            let mut prev_event_ts = None;
            let mut prev_day_divider_ts = None;

            for (i, item) in items.iter().enumerate() {
                if let Some(ev) = item.as_event() {
                    let ts = ev.timestamp();

                    // We have the same date as the previous event we've seen.
                    if let Some(prev_ts) = prev_event_ts {
                        if !is_same_date_as(prev_ts, ts) {
                            report.errors.push(
                                DayDividerInsertError::MissingDayDividerBetweenEvents { at: i },
                            );
                        }
                    }

                    // There is a day divider before us, and it's the same date as our timestamp.
                    if let Some(prev_ts) = prev_day_divider_ts {
                        if !is_same_date_as(prev_ts, ts) {
                            report.errors.push(
                                DayDividerInsertError::InconsistentDateAfterPreviousDayDivider {
                                    at: i,
                                },
                            );
                        }
                    } else {
                        report
                            .errors
                            .push(DayDividerInsertError::MissingDayDividerBeforeEvent { at: i });
                    }

                    prev_event_ts = Some(ts);
                } else if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) =
                    item.kind()
                {
                    // The previous day divider is for a different date.
                    if let Some(prev_ts) = prev_day_divider_ts {
                        if is_same_date_as(prev_ts, *ts) {
                            report
                                .errors
                                .push(DayDividerInsertError::DuplicateDayDivider { at: i });
                        }
                    }

                    prev_event_ts = None;
                    prev_day_divider_ts = Some(*ts);
                }
            }
        }

        if report.errors.is_empty() {
            None
        } else {
            Some(report)
        }
    }
}

#[derive(Debug)]
enum DayDividerOperation {
    Insert(usize, MilliSecondsSinceUnixEpoch),
    Replace(usize, MilliSecondsSinceUnixEpoch),
    Remove(usize),
}

/// Returns whether the two dates for the given timestamps are the same or not.
#[inline]
fn is_same_date_as(lhs: MilliSecondsSinceUnixEpoch, rhs: MilliSecondsSinceUnixEpoch) -> bool {
    timestamp_to_date(lhs) == timestamp_to_date(rhs)
}

/// A report returned by [`DayDividerAdjuster::check_invariants`].
struct DayDividerInvariantsReport<'a, 'o> {
    /// Initial state before inserting the items.
    initial_state: Option<Vec<Arc<TimelineItem>>>,
    /// The operations that have been applied on the list.
    operations: Vec<DayDividerOperation>,
    /// Final state after inserting the day dividers.
    final_state: &'a ObservableVectorTransaction<'o, Arc<TimelineItem>>,
    /// Errors encountered in the algorithm.
    errors: Vec<DayDividerInsertError>,
}

impl<'a, 'o> Display for DayDividerInvariantsReport<'a, 'o> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(initial_state) = &self.initial_state {
            writeln!(f, "Initial state:")?;

            for (i, item) in initial_state.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) = item.kind()
                {
                    writeln!(f, "#{i} --- {}", ts.0)?;
                } else if let Some(event) = item.as_event() {
                    // id: timestamp
                    writeln!(f, "#{i} {}: {}", event.event_id().unwrap(), event.timestamp().0)?;
                } else {
                    writeln!(f, "#{i} (other virtual item)")?;
                }
            }

            writeln!(f, "\nOperations to apply:")?;
            for op in &self.operations {
                match *op {
                    DayDividerOperation::Insert(i, ts) => writeln!(f, "insert @ {i}: {}", ts.0)?,
                    DayDividerOperation::Replace(i, ts) => writeln!(f, "replace @ {i}: {}", ts.0)?,
                    DayDividerOperation::Remove(i) => writeln!(f, "remove @ {i}")?,
                }
            }

            writeln!(f, "\nFinal state:")?;
            for (i, item) in self.final_state.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) = item.kind()
                {
                    writeln!(f, "#{i} --- {}", ts.0)?;
                } else if let Some(event) = item.as_event() {
                    // id: timestamp
                    writeln!(f, "#{i} {}: {}", event.event_id().unwrap(), event.timestamp().0)?;
                } else {
                    writeln!(f, "#{i} (other virtual item)")?;
                }
            }

            writeln!(f)?;
        }

        for err in &self.errors {
            writeln!(f, "{err}")?;
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum DayDividerInsertError {
    /// The first item isn't a day divider.
    #[error("The first item isn't a day divider")]
    FirstItemNotDayDivider,

    /// There are two day dividers for the same date.
    #[error("Duplicate day divider @ {at}.")]
    DuplicateDayDivider { at: usize },

    /// The last item is a day divider.
    #[error("The last item is a day divider.")]
    TrailingDayDivider,

    /// Two events are following each other but they have different dates
    /// without a day divider between them.
    #[error("Missing day divider between events @ {at}")]
    MissingDayDividerBetweenEvents { at: usize },

    /// Some event is missing a day divider before it.
    #[error("Missing day divider before event @ {at}")]
    MissingDayDividerBeforeEvent { at: usize },

    /// An event and the previous day divider aren't focused on the same date.
    #[error("Event @ {at} and the previous day divider aren't targeting the same date")]
    InconsistentDateAfterPreviousDayDivider { at: usize },
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
