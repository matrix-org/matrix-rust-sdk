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
use indexmap::{map::Entry, IndexMap};
use matrix_sdk::{
    crypto::types::events::UtdCause, deserialized_responses::EncryptionInfo,
    send_queue::AbortSendHandle,
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
    html::RemoveReplyFallback,
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId,
};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};

use super::{
    day_dividers::DayDividerAdjuster,
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EventItemIdentifier, EventSendState,
        EventTimelineItemKind, LocalEventTimelineItem, Profile, RemoteEventOrigin,
        RemoteEventTimelineItem,
    },
    inner::{TimelineInnerMetadata, TimelineInnerStateTransaction},
    polls::PollState,
    util::{rfind_event_by_id, rfind_event_item},
    EventTimelineItem, InReplyToDetails, Message, OtherState, ReactionGroup, ReactionSenderData,
    Sticker, TimelineDetails, TimelineItem, TimelineItemContent,
};
use crate::{events::SyncTimelineEventWithoutContent, DEFAULT_SANITIZER_MODE};

/// When adding an event, useful information related to the source of the event.
#[derive(Clone)]
pub(super) enum Flow {
    /// The event was locally created.
    Local {
        /// The transaction id we've used in requests associated to this event.
        txn_id: OwnedTransactionId,

        /// A handle to abort sending this event.
        abort_handle: Option<AbortSendHandle>,
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

    /// A redaction of a local echo.
    LocalRedaction { redacts: OwnedTransactionId },

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
    is_live_timeline: bool,
}

impl<'a, 'o> TimelineEventHandler<'a, 'o> {
    pub(super) fn new(
        state: &'a mut TimelineInnerStateTransaction<'o>,
        ctx: TimelineEventContext,
    ) -> Self {
        let TimelineInnerStateTransaction { items, meta, is_live_timeline, .. } = state;
        Self {
            items,
            meta,
            ctx,
            is_live_timeline: *is_live_timeline,
            result: HandleEventResult::default(),
        }
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
        raw_event: Option<&Raw<AnySyncTimelineEvent>>,
    ) -> HandleEventResult {
        let span = tracing::Span::current();

        day_divider_adjuster.mark_used();

        let should_add = match &self.ctx.flow {
            Flow::Local { txn_id, .. } => {
                span.record("txn_id", debug(txn_id));
                debug!("Handling local event");

                // Only add new timeline items if we're in the live mode, i.e. not in the
                // event-focused mode.
                self.is_live_timeline
            }

            Flow::Remote { event_id, txn_id, position, should_add, .. } => {
                span.record("event_id", debug(event_id));
                span.record("position", debug(position));
                if let Some(txn_id) = txn_id {
                    span.record("txn_id", debug(txn_id));
                }
                trace!("Handling remote event");

                // Retrieve the origin of the event.
                let origin = match position {
                    TimelineItemPosition::End { origin }
                    | TimelineItemPosition::Start { origin } => *origin,

                    TimelineItemPosition::Update(idx) => self
                        .items
                        .get(*idx)
                        .and_then(|item| item.as_event())
                        .and_then(|item| item.as_remote())
                        .map_or(RemoteEventOrigin::Unknown, |item| item.origin),
                };

                match origin {
                    RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => {
                        // If the event comes the sync (or is unknown), consider adding it only if
                        // the timeline is in live mode; we don't want to display arbitrary sync
                        // events in an event-focused timeline.
                        self.is_live_timeline && *should_add
                    }
                    RemoteEventOrigin::Pagination | RemoteEventOrigin::Cache => {
                        // Otherwise, forward the previous decision to add it.
                        *should_add
                    }
                }
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
                    if should_add {
                        self.add_item(TimelineItemContent::message(c, relations, self.items));
                    }
                }
                AnyMessageLikeEventContent::RoomEncrypted(c) => {
                    // TODO: Handle replacements if the replaced event is also UTD
                    let cause = UtdCause::determine(raw_event);
                    self.add_item(TimelineItemContent::unable_to_decrypt(c, cause));

                    // Let the hook know that we ran into an unable-to-decrypt that is added to the
                    // timeline.
                    if let Some(hook) = self.meta.unable_to_decrypt_hook.as_ref() {
                        if let Flow::Remote { event_id, .. } = &self.ctx.flow {
                            hook.on_utd(event_id, cause).await;
                        }
                    }
                }
                AnyMessageLikeEventContent::Sticker(content) => {
                    if should_add {
                        self.add_item(TimelineItemContent::Sticker(Sticker { content }));
                    }
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
                    if should_add {
                        self.add_item(TimelineItemContent::CallInvite);
                    }
                }
                AnyMessageLikeEventContent::CallNotify(_) => {
                    if should_add {
                        self.add_item(TimelineItemContent::CallNotify)
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
                    self.add_item(TimelineItemContent::RedactedMessage);
                }
            }

            TimelineEventKind::Redaction { redacts } => {
                self.handle_redaction(redacts);
            }
            TimelineEventKind::LocalRedaction { redacts } => {
                self.handle_local_redaction(redacts);
            }

            TimelineEventKind::RoomMember { user_id, content, sender } => {
                if should_add {
                    self.add_item(TimelineItemContent::room_member(user_id, content, sender));
                }
            }

            TimelineEventKind::OtherState { state_key, content } => {
                if should_add {
                    self.add_item(TimelineItemContent::OtherState(OtherState {
                        state_key,
                        content,
                    }));
                }
            }

            TimelineEventKind::FailedToParseMessageLike { event_type, error } => {
                if should_add {
                    self.add_item(TimelineItemContent::FailedToParseMessageLike {
                        event_type,
                        error,
                    });
                }
            }

            TimelineEventKind::FailedToParseState { event_type, state_key, error } => {
                if should_add {
                    self.add_item(TimelineItemContent::FailedToParseState {
                        event_type,
                        state_key,
                        error,
                    });
                }
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
                mentions: replacement.new_content.mentions,
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

        if should_add {
            self.add_item(TimelineItemContent::Poll(poll_state));
        }
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

    /// Looks for the redacted event in all the timeline event items, and
    /// redacts it.
    ///
    /// This only applies to *remote* events; for local items being redacted,
    /// use [`Self::handle_local_redaction`].
    ///
    /// This assumes the redacted event was present in the timeline in the first
    /// place; it will warn if the redacted event has not been found.
    #[instrument(skip_all, fields(redacts_event_id = ?redacts))]
    fn handle_redaction(&mut self, redacts: OwnedEventId) {
        // TODO: Apply local redaction of PollResponse and PollEnd events.
        // https://github.com/matrix-org/matrix-rust-sdk/pull/2381#issuecomment-1689647825

        let id = EventItemIdentifier::EventId(redacts.clone());

        // If it's a reaction that's being redacted, handle it here.
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

        // General path: redact another kind of (non-reaction) event.
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

        // Look for any timeline event that's a reply to the redacted event, and redact
        // the replied-to event there as well.
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
    fn handle_local_redaction(&mut self, redacts: OwnedTransactionId) {
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
    fn add_item(&mut self, content: TimelineItemContent) {
        self.result.item_added = true;

        let sender = self.ctx.sender.to_owned();
        let sender_profile = TimelineDetails::from_initial_value(self.ctx.sender_profile.clone());
        let timestamp = self.ctx.timestamp;
        let mut reactions = self.pending_reactions().unwrap_or_default();

        let kind: EventTimelineItemKind = match &self.ctx.flow {
            Flow::Local { txn_id, abort_handle } => LocalEventTimelineItem {
                send_state: EventSendState::NotSentYet,
                transaction_id: txn_id.to_owned(),
                abort_handle: abort_handle.clone(),
            }
            .into(),

            Flow::Remote { event_id, raw_event, position, .. } => {
                // Drop pending reactions if the message is redacted.
                if let TimelineItemContent::RedactedMessage = content {
                    if !reactions.is_empty() {
                        reactions = BundledReactions::default();
                    }
                }

                let origin = match *position {
                    TimelineItemPosition::Start { origin }
                    | TimelineItemPosition::End { origin } => origin,

                    // For updates, reuse the origin of the encrypted event.
                    #[cfg(feature = "e2e-encryption")]
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
                        self.items.set(idx, TimelineItem::new(item, old_item_id.to_owned()));
                        return;
                    }

                    // In more complex cases, remove the item before re-adding the item.
                    trace!("Removing local echo or duplicate timeline item");
                    removed_event_item_id = Some(self.items.remove(idx).internal_id.clone());

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

            #[cfg(feature = "e2e-encryption")]
            Flow::Remote { position: TimelineItemPosition::Update(idx), .. } => {
                trace!("Updating timeline item at position {idx}");
                let id = self.items[*idx].internal_id.clone();
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
                self.items.set(idx, TimelineItem::new(new_item, item.internal_id.to_owned()));
                self.result.items_updated += 1;
            }
            true
        } else {
            false
        }
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
