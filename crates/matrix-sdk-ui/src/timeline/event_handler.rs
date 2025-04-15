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

use std::{ops::ControlFlow, sync::Arc};

use as_variant::as_variant;
use indexmap::IndexMap;
use matrix_sdk::{
    crypto::types::events::UtdCause,
    deserialized_responses::{EncryptionInfo, UnableToDecryptInfo},
    ring_buffer::RingBuffer,
    send_queue::SendHandle,
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
            encrypted::RoomEncryptedEventContent,
            member::RoomMemberEventContent,
            message::{Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
        },
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, BundledMessageLikeRelations, EventContent, FullStateEventContent,
        MessageLikeEventType, StateEventType, SyncStateEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    TransactionId,
};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};

use super::{
    algorithms::{rfind_event_by_id, rfind_event_by_item_id},
    controller::{
        find_item_and_apply_aggregation, Aggregation, AggregationKind, ApplyAggregationResult,
        ObservableItemsTransaction, PendingEdit, PendingEditKind, TimelineMetadata,
        TimelineStateTransaction,
    },
    date_dividers::DateDividerAdjuster,
    event_item::{
        extract_bundled_edit_event_json, extract_poll_edit_content, extract_room_msg_edit_content,
        AnyOtherFullStateEventContent, EventSendState, EventTimelineItemKind,
        LocalEventTimelineItem, PollState, Profile, RemoteEventOrigin, RemoteEventTimelineItem,
        TimelineEventItemId,
    },
    traits::RoomDataProvider,
    EncryptedMessage, EventTimelineItem, InReplyToDetails, MsgLikeContent, MsgLikeKind, OtherState,
    ReactionStatus, RepliedToEvent, Sticker, TimelineDetails, TimelineItem, TimelineItemContent,
};
use crate::events::SyncTimelineEventWithoutContent;

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

    /// Returns the [`TimelineEventItemId`] associated to this future item.
    pub(crate) fn timeline_item_id(&self) -> TimelineEventItemId {
        match self {
            Flow::Remote { event_id, .. } => TimelineEventItemId::EventId(event_id.clone()),
            Flow::Local { txn_id, .. } => TimelineEventItemId::TransactionId(txn_id.clone()),
        }
    }

    /// If the flow is remote, returns the associated full raw event.
    pub(crate) fn raw_event(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        as_variant!(self, Flow::Remote { raw_event, .. } => raw_event)
    }
}

pub(super) struct TimelineEventContext {
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: Option<Profile>,
    /// The event's `origin_server_ts` field (or creation time for local echo).
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

    /// An encrypted event that could not be decrypted
    UnableToDecrypt { content: RoomEncryptedEventContent, utd_cause: UtdCause },

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
    /// Creates a new `TimelineEventKind`.
    ///
    /// # Arguments
    ///
    /// * `event` - The event for which we should create a `TimelineEventKind`.
    /// * `raw_event` - The [`Raw`] JSON for `event`. (Required so that we can
    ///   access `unsigned` data.)
    /// * `room_data_provider` - An object which will provide information about
    ///   the room containing the event.
    /// * `unable_to_decrypt_info` - If `event` represents a failure to decrypt,
    ///   information about that failure. Otherwise, `None`.
    pub async fn from_event<P: RoomDataProvider>(
        event: AnySyncTimelineEvent,
        raw_event: &Raw<AnySyncTimelineEvent>,
        room_data_provider: &P,
        unable_to_decrypt_info: Option<UnableToDecryptInfo>,
    ) -> Self {
        let room_version = room_data_provider.room_version();
        match event {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) => {
                if let Some(redacts) = ev.redacts(&room_version).map(ToOwned::to_owned) {
                    Self::Redaction { redacts }
                } else {
                    Self::RedactedMessage { event_type: ev.event_type() }
                }
            }
            AnySyncTimelineEvent::MessageLike(ev) => match ev.original_content() {
                Some(AnyMessageLikeEventContent::RoomEncrypted(content)) => {
                    // An event which is still encrypted.
                    if let Some(unable_to_decrypt_info) = unable_to_decrypt_info {
                        let utd_cause = UtdCause::determine(
                            raw_event,
                            room_data_provider.crypto_context_info().await,
                            &unable_to_decrypt_info,
                        );
                        Self::UnableToDecrypt { content, utd_cause }
                    } else {
                        // If we get here, it means that some part of the code has created a
                        // `TimelineEvent` containing an `m.room.encrypted` event
                        // without decrypting it. Possibly this means that encryption has not been
                        // configured.
                        // We treat it the same as any other message-like event.
                        Self::Message {
                            content: AnyMessageLikeEventContent::RoomEncrypted(content),
                            relations: ev.relations(),
                        }
                    }
                }
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
    Start {
        /// The origin of the new item(s).
        origin: RemoteEventOrigin,
    },

    /// One or more items are appended to the timeline (i.e. they're the most
    /// recent).
    End {
        /// The origin of the new item(s).
        origin: RemoteEventOrigin,
    },

    /// One item is inserted to the timeline.
    At {
        /// Where to insert the remote event.
        event_index: usize,

        /// The origin of the new item.
        origin: RemoteEventOrigin,
    },

    /// A single item is updated.
    ///
    /// This can happen for instance after a UTD has been successfully
    /// decrypted, or when it's been redacted at the source.
    UpdateAt {
        /// The index of the **timeline item**.
        timeline_item_index: usize,
    },
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
    items: &'a mut ObservableItemsTransaction<'o>,
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
        date_divider_adjuster: &mut DateDividerAdjuster,
        event_kind: TimelineEventKind,
    ) -> HandleEventResult {
        let span = tracing::Span::current();

        date_divider_adjuster.mark_used();

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
                    relates_to: Some(Relation::Replacement(re)),
                    ..
                }) => {
                    self.handle_room_message_edit(re);
                }

                AnyMessageLikeEventContent::RoomMessage(c) => {
                    if should_add {
                        self.handle_room_message(c, relations);
                    }
                }

                AnyMessageLikeEventContent::Sticker(content) => {
                    if should_add {
                        self.add_item(
                            TimelineItemContent::MsgLike(MsgLikeContent {
                                kind: MsgLikeKind::Sticker(Sticker { content }),
                                reactions: Default::default(),
                                thread_root: None,
                                in_reply_to: None,
                                thread_summary: None,
                            }),
                            None,
                        );
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

            TimelineEventKind::UnableToDecrypt { content, utd_cause } => {
                // TODO: Handle replacements if the replaced event is also UTD
                if should_add {
                    self.add_item(
                        TimelineItemContent::MsgLike(MsgLikeContent::unable_to_decrypt(
                            EncryptedMessage::from_content(content, utd_cause),
                        )),
                        None,
                    );
                }

                // Let the hook know that we ran into an unable-to-decrypt that is added to the
                // timeline.
                if let Some(hook) = self.meta.unable_to_decrypt_hook.as_ref() {
                    if let Some(event_id) = &self.ctx.flow.event_id() {
                        hook.on_utd(event_id, utd_cause, self.ctx.timestamp, &self.ctx.sender)
                            .await;
                    }
                }
            }

            TimelineEventKind::RedactedMessage { event_type } => {
                if event_type != MessageLikeEventType::Reaction && should_add {
                    self.add_item(TimelineItemContent::MsgLike(MsgLikeContent::redacted()), None);
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

            if let Flow::Remote {
                position: TimelineItemPosition::UpdateAt { timeline_item_index },
                ..
            } = self.ctx.flow
            {
                // If add was not called, that means the UTD event is one that
                // wouldn't normally be visible. Remove it.
                trace!("Removing UTD that was successfully retried");
                self.items.remove(timeline_item_index);

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

        let mut replied_to_event_id = None;
        let mut thread_root = None;
        let in_reply_to_details = msg.relates_to.as_ref().and_then(|relation| match relation {
            Relation::Reply { in_reply_to } => {
                replied_to_event_id = Some(in_reply_to.event_id.clone());
                Some(InReplyToDetails::new(in_reply_to.event_id.clone(), self.items))
            }
            Relation::Thread(thread) => {
                thread_root = Some(thread.event_id.clone());
                thread.in_reply_to.as_ref().map(|in_reply_to| {
                    replied_to_event_id = Some(in_reply_to.event_id.clone());
                    InReplyToDetails::new(in_reply_to.event_id.clone(), self.items)
                })
            }
            _ => None,
        });

        // If this message is a reply to another message, add an entry in the inverted
        // mapping.
        if let Some(event_id) = self.ctx.flow.event_id() {
            if let Some(replied_to_event_id) = replied_to_event_id {
                // This is a reply! Add an entry.
                self.meta
                    .replies
                    .entry(replied_to_event_id)
                    .or_default()
                    .insert(event_id.to_owned());
            }
        }

        let edit_json = edit_json.flatten();

        self.add_item(
            TimelineItemContent::message(
                msg,
                edit_content,
                Default::default(),
                thread_root,
                in_reply_to_details,
                None,
            ),
            edit_json,
        );
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
                Self::maybe_update_responses(
                    self.meta,
                    self.items,
                    &replacement.event_id,
                    &new_item,
                );

                // Update the event itself.
                self.items.replace(item_pos, TimelineItem::new(new_item, internal_id));
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
            TimelineItemPosition::Start { .. }
            | TimelineItemPosition::At { .. }
            | TimelineItemPosition::UpdateAt { .. } => {
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

        let TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Message(msg),
            reactions,
            thread_root,
            in_reply_to,
            thread_summary,
        }) = item.content()
        else {
            info!(
                "Edit of message event applies to {:?}, discarding",
                item.content().debug_string(),
            );
            return None;
        };

        let mut new_msg = msg.clone();
        new_msg.apply_edit(new_content);

        let mut new_item = item.with_content_and_latest_edit(
            TimelineItemContent::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::Message(new_msg),
                reactions: reactions.clone(),
                thread_root: thread_root.clone(),
                in_reply_to: in_reply_to.clone(),
                thread_summary: thread_summary.clone(),
            }),
            edit_json,
        );

        if let Flow::Remote { encryption_info, .. } = &self.ctx.flow {
            new_item = new_item.with_encryption_info(encryption_info.clone());
        }

        Some(new_item)
    }

    /// Apply a reaction to a *remote* event.
    ///
    /// Reactions to local events are applied in
    /// [`crate::timeline::TimelineController::handle_local_echo`].
    #[instrument(skip_all, fields(relates_to_event_id = ?c.relates_to.event_id))]
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let target = TimelineEventItemId::EventId(c.relates_to.event_id);

        // Add the aggregation to the manager.
        let reaction_status = match &self.ctx.flow {
            Flow::Local { send_handle, .. } => {
                // This is a local echo for a reaction to a remote event.
                ReactionStatus::LocalToRemote(send_handle.clone())
            }
            Flow::Remote { event_id, .. } => {
                // This is the remote echo for a reaction to a remote event.
                ReactionStatus::RemoteToRemote(event_id.clone())
            }
        };
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::Reaction {
                key: c.relates_to.key,
                sender: self.ctx.sender.clone(),
                timestamp: self.ctx.timestamp,
                reaction_status,
            },
        );

        self.meta.aggregations.add(target.clone(), aggregation.clone());
        if find_item_and_apply_aggregation(self.items, &target, aggregation) {
            self.result.items_updated += 1;
        }
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
        self.items.replace(item_pos, TimelineItem::new(new_item, item.internal_id.to_owned()));
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

        let TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Poll(poll_state),
            reactions,
            thread_root,
            in_reply_to,
            thread_summary,
        }) = &item.content()
        else {
            info!("Edit of poll event applies to {}, discarding", item.content().debug_string(),);
            return None;
        };

        let new_content = match poll_state.edit(replacement.new_content) {
            Some(edited_poll_state) => TimelineItemContent::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::Poll(edited_poll_state),
                reactions: reactions.clone(),
                thread_root: thread_root.clone(),
                in_reply_to: in_reply_to.clone(),
                thread_summary: thread_summary.clone(),
            }),
            None => {
                info!("Not applying edit to a poll that's already ended");
                return None;
            }
        };

        Some(item.with_content_and_latest_edit(new_content, edit_json))
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

        let poll_state = PollState::new(c, edit_content);

        let edit_json = edit_json.flatten();

        self.add_item(
            TimelineItemContent::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::Poll(poll_state),
                reactions: Default::default(),
                thread_root: None,
                in_reply_to: None,
                thread_summary: None,
            }),
            edit_json,
        );
    }

    fn handle_poll_response(&mut self, c: UnstablePollResponseEventContent) {
        let target = TimelineEventItemId::EventId(c.relates_to.event_id);
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::PollResponse {
                sender: self.ctx.sender.clone(),
                timestamp: self.ctx.timestamp,
                answers: c.poll_response.answers,
            },
        );
        self.meta.aggregations.add(target.clone(), aggregation.clone());
        if find_item_and_apply_aggregation(self.items, &target, aggregation) {
            self.result.items_updated += 1;
        }
    }

    fn handle_poll_end(&mut self, c: UnstablePollEndEventContent) {
        let target = TimelineEventItemId::EventId(c.relates_to.event_id);
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::PollEnd { end_date: self.ctx.timestamp },
        );
        self.meta.aggregations.add(target.clone(), aggregation.clone());
        if find_item_and_apply_aggregation(self.items, &target, aggregation) {
            self.result.items_updated += 1;
        }
    }

    /// Looks for the redacted event in all the timeline event items, and
    /// redacts it.
    ///
    /// This assumes the redacted event was present in the timeline in the first
    /// place; it will warn if the redacted event has not been found.
    #[instrument(skip_all, fields(redacts_event_id = ?redacted))]
    fn handle_redaction(&mut self, redacted: OwnedEventId) {
        // TODO: Apply local redaction of PollResponse and PollEnd events.
        // https://github.com/matrix-org/matrix-rust-sdk/pull/2381#issuecomment-1689647825

        // If it's an aggregation that's being redacted, handle it here.
        if self.handle_aggregation_redaction(redacted.clone()) {
            // When we have raw timeline items, we should not return here anymore, as we
            // might need to redact the raw item as well.
            return;
        }

        // General path: redact another kind of (non-reaction) event.
        if let Some((idx, item)) = rfind_event_by_id(self.items, &redacted) {
            if item.as_remote().is_some() {
                if item.content.is_redacted() {
                    debug!("event item is already redacted");
                } else {
                    let new_item = item.redact(&self.meta.room_version);
                    let internal_id = item.internal_id.to_owned();

                    // Look for any timeline event that's a reply to the redacted event, and redact
                    // the replied-to event there as well.
                    Self::maybe_update_responses(self.meta, self.items, &redacted, &new_item);

                    self.items.replace(idx, TimelineItem::new(new_item, internal_id));
                    self.result.items_updated += 1;
                }
            } else {
                error!("inconsistent state: redaction received on a non-remote event item");
            }
        } else {
            debug!("Timeline item not found, discarding redaction");
        };
    }

    /// Attempts to redact an aggregation (e.g. a reaction, a poll response,
    /// etc.).
    ///
    /// Returns true if it's succeeded.
    #[instrument(skip_all, fields(redacts = ?aggregation_id))]
    fn handle_aggregation_redaction(&mut self, aggregation_id: OwnedEventId) -> bool {
        let aggregation_id = TimelineEventItemId::EventId(aggregation_id);

        let Some((target, aggregation)) =
            self.meta.aggregations.try_remove_aggregation(&aggregation_id)
        else {
            // This wasn't a known aggregation that was redacted.
            return false;
        };

        if let Some((item_pos, item)) = rfind_event_by_item_id(self.items, target) {
            let mut content = item.content().clone();
            match aggregation.unapply(&mut content) {
                ApplyAggregationResult::UpdatedItem => {
                    trace!("removed aggregation");
                    let internal_id = item.internal_id.to_owned();
                    let new_item = item.with_content(content);
                    self.items.replace(item_pos, TimelineItem::new(new_item, internal_id));
                    self.result.items_updated += 1;
                }
                ApplyAggregationResult::LeftItemIntact => {}
                ApplyAggregationResult::Error(err) => {
                    warn!("error when unapplying aggregation: {err}");
                }
            }
        } else {
            info!("missing related-to item ({target:?}) for aggregation {aggregation_id:?}");
        }

        // In all cases, we noticed this was an aggregation.
        true
    }

    /// Add a new event item in the timeline.
    ///
    /// # Safety
    ///
    /// This method is not marked as unsafe **but** it manipulates
    /// [`ObservableItemsTransaction::all_remote_events`]. 2 rules **must** be
    /// respected:
    ///
    /// 1. the remote event of the item being added **must** be present in
    ///    `all_remote_events`,
    /// 2. the lastly added or updated remote event must be associated to the
    ///    timeline item being added here.
    fn add_item(
        &mut self,
        mut content: TimelineItemContent,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) {
        self.result.item_added = true;

        // Apply any pending or stashed aggregations.
        if let Err(err) =
            self.meta.aggregations.apply(&self.ctx.flow.timeline_item_id(), &mut content)
        {
            warn!("discarding aggregations: {err}");
        }

        let sender = self.ctx.sender.to_owned();
        let sender_profile = TimelineDetails::from_initial_value(self.ctx.sender_profile.clone());
        let timestamp = self.ctx.timestamp;

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
                    | TimelineItemPosition::End { origin }
                    | TimelineItemPosition::At { origin, .. } => origin,

                    // For updates, reuse the origin of the encrypted event.
                    TimelineItemPosition::UpdateAt { timeline_item_index: idx } => self.items[idx]
                        .as_event()
                        .and_then(|ev| Some(ev.as_remote()?.origin))
                        .unwrap_or_else(|| {
                            error!("Tried to update a local event");
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

        let is_room_encrypted = self.meta.is_room_encrypted;

        let item = EventTimelineItem::new(
            sender,
            sender_profile,
            timestamp,
            content,
            kind,
            is_room_encrypted,
        );

        match &self.ctx.flow {
            Flow::Local { .. } => {
                trace!("Adding new local timeline item");

                let item = self.meta.new_timeline_item(item);

                self.items.push_back(item, None);
            }

            Flow::Remote {
                position: TimelineItemPosition::Start { .. }, event_id, txn_id, ..
            } => {
                let item = Self::recycle_local_or_create_item(
                    self.items,
                    self.meta,
                    item,
                    event_id,
                    txn_id.as_deref(),
                );

                trace!("Adding new remote timeline item at the start");

                self.items.push_front(item, Some(0));
            }

            Flow::Remote {
                position: TimelineItemPosition::At { event_index, .. },
                event_id,
                txn_id,
                ..
            } => {
                let item = Self::recycle_local_or_create_item(
                    self.items,
                    self.meta,
                    item,
                    event_id,
                    txn_id.as_deref(),
                );

                let all_remote_events = self.items.all_remote_events();
                let event_index = *event_index;

                // Look for the closest `timeline_item_index` at the left of `event_index`.
                let timeline_item_index = all_remote_events
                    .range(0..=event_index)
                    .rev()
                    .find_map(|event_meta| event_meta.timeline_item_index)
                    // The new `timeline_item_index` is the previous + 1.
                    .map(|timeline_item_index| timeline_item_index + 1);

                // No index? Look for the closest `timeline_item_index` at the right of
                // `event_index`.
                let timeline_item_index = timeline_item_index.or_else(|| {
                    all_remote_events
                        .range(event_index + 1..)
                        .find_map(|event_meta| event_meta.timeline_item_index)
                });

                // Still no index? Well, it means there is no existing `timeline_item_index`
                // so we are inserting at the last non-local item position as a fallback.
                let timeline_item_index = timeline_item_index.unwrap_or_else(|| {
                    self.items
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(timeline_item_index, timeline_item)| {
                            (!timeline_item.as_event()?.is_local_echo())
                                .then_some(timeline_item_index + 1)
                        })
                        .unwrap_or_else(|| {
                            // We don't have any local echo, so we could insert at 0. However, in
                            // the case of an insertion caused by a pagination, we
                            // may have already pushed the start of the timeline item, so we need
                            // to check if the first item is that, and insert after it otherwise.
                            if self.items.get(0).is_some_and(|item| item.is_timeline_start()) {
                                1
                            } else {
                                0
                            }
                        })
                });

                trace!(
                    ?event_index,
                    ?timeline_item_index,
                    "Adding new remote timeline at specific event index"
                );

                self.items.insert(timeline_item_index, item, Some(event_index));
            }

            Flow::Remote {
                position: TimelineItemPosition::End { .. }, event_id, txn_id, ..
            } => {
                let item = Self::recycle_local_or_create_item(
                    self.items,
                    self.meta,
                    item,
                    event_id,
                    txn_id.as_deref(),
                );

                // Local events are always at the bottom. Let's find the latest remote event
                // and insert after it, otherwise, if there is no remote event, insert at 0.
                let timeline_item_index = self
                    .items
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(timeline_item_index, timeline_item)| {
                        (!timeline_item.as_event()?.is_local_echo())
                            .then_some(timeline_item_index + 1)
                    })
                    .unwrap_or(0);

                let event_index = self
                    .items
                    .all_remote_events()
                    .last_index()
                    // The last remote event is necessarily associated to this
                    // timeline item, see the contract of this method. Let's fallback to a similar
                    // value as `timeline_item_index` instead of panicking.
                    .or_else(|| {
                        error!(?event_id, "Failed to read the last event index from `AllRemoteEvents`: at least one event must be present");

                        Some(0)
                    });

                // Try to keep precise insertion semantics here, in this exact order:
                //
                // * _push back_ when the new item is inserted after all items (the assumption
                // being that this is the hot path, because most of the time new events
                // come from the sync),
                // * _push front_ when the new item is inserted at index 0,
                // * _insert_ otherwise.

                if timeline_item_index == self.items.len() {
                    trace!("Adding new remote timeline item at the back");
                    self.items.push_back(item, event_index);
                } else if timeline_item_index == 0 {
                    trace!("Adding new remote timeline item at the front");
                    self.items.push_front(item, event_index);
                } else {
                    trace!(
                        timeline_item_index,
                        "Adding new remote timeline item at specific index"
                    );
                    self.items.insert(timeline_item_index, item, event_index);
                }
            }

            Flow::Remote {
                event_id: decrypted_event_id,
                position: TimelineItemPosition::UpdateAt { timeline_item_index: idx },
                ..
            } => {
                trace!("Updating timeline item at position {idx}");

                // Update all events that replied to this previously encrypted message.
                Self::maybe_update_responses(self.meta, self.items, decrypted_event_id, &item);

                let internal_id = self.items[*idx].internal_id.clone();
                self.items.replace(*idx, TimelineItem::new(item, internal_id));
            }
        }

        // If we don't have a read marker item, look if we need to add one now.
        if !self.meta.has_up_to_date_read_marker_item {
            self.meta.update_read_marker(self.items);
        }
    }

    /// Try to recycle a local timeline item for the same event, or create a new
    /// timeline item for it.
    ///
    /// Note: this method doesn't take `&mut self` to avoid a borrow checker
    /// conflict with `TimelineEventHandler::add_item`.
    fn recycle_local_or_create_item(
        items: &mut ObservableItemsTransaction<'_>,
        meta: &mut TimelineMetadata,
        mut new_item: EventTimelineItem,
        event_id: &EventId,
        transaction_id: Option<&TransactionId>,
    ) -> Arc<TimelineItem> {
        // Detect a local timeline item that matches `event_id` or `transaction_id`.
        if let Some((local_timeline_item_index, local_timeline_item)) = items
            .iter()
            // Get the index of each item.
            .enumerate()
            // Iterate from the end to the start.
            .rev()
            // Use a `Iterator::try_fold` to produce a single value, and to stop the iterator
            // when a non local event timeline item is met. We want to stop iterating when:
            //
            // - a duplicate local event timeline item has been found,
            // - a non local event timeline item is met,
            // - a non event timeline is met.
            //
            // Indeed, it is a waste of time to iterate over all items in `items`. Local event
            // timeline items are necessarily at the end of `items`: as soon as they have been
            // iterated, we can stop the entire iteration.
            .try_fold((), |(), (nth, timeline_item)| {
                let Some(event_timeline_item) = timeline_item.as_event() else {
                    // Not an event timeline item? Stop iterating here.
                    return ControlFlow::Break(None);
                };

                // Not a local event timeline item? Stop iterating here.
                if !event_timeline_item.is_local_echo() {
                    return ControlFlow::Break(None);
                }

                if Some(event_id) == event_timeline_item.event_id()
                    || (transaction_id.is_some()
                        && transaction_id == event_timeline_item.transaction_id())
                {
                    // A duplicate local event timeline item has been found!
                    ControlFlow::Break(Some((nth, event_timeline_item)))
                } else {
                    // This local event timeline is not the one we are looking for. Continue our
                    // search.
                    ControlFlow::Continue(())
                }
            })
            .break_value()
            .flatten()
        {
            trace!(
                ?event_id,
                ?transaction_id,
                ?local_timeline_item_index,
                "Removing local timeline item"
            );

            transfer_details(&mut new_item, local_timeline_item);

            // Remove the local timeline item.
            let recycled = items.remove(local_timeline_item_index);
            TimelineItem::new(new_item, recycled.internal_id.clone())
        } else {
            // We haven't found a matching local item to recycle; create a new item.
            meta.new_timeline_item(new_item)
        }
    }

    /// After updating the timeline item `new_item` which id is
    /// `target_event_id`, update other items that are responses to this item.
    fn maybe_update_responses(
        meta: &mut TimelineMetadata,
        items: &mut ObservableItemsTransaction<'_>,
        target_event_id: &EventId,
        new_item: &EventTimelineItem,
    ) {
        let Some(replies) = meta.replies.get(target_event_id) else {
            trace!("item has no replies");
            return;
        };

        for reply_id in replies {
            let Some(timeline_item_index) = items
                .get_remote_event_by_event_id(reply_id)
                .and_then(|meta| meta.timeline_item_index)
            else {
                warn!(%reply_id, "event not known as an item in the timeline");
                continue;
            };

            let Some(item) = items.get(timeline_item_index) else {
                warn!(%reply_id, timeline_item_index, "mapping from event id to timeline item likely incorrect");
                continue;
            };

            let Some(event_item) = item.as_event() else { continue };
            let Some(msglike) = event_item.content.as_msglike() else { continue };
            let Some(message) = msglike.as_message() else { continue };
            let Some(in_reply_to) = msglike.in_reply_to.as_ref() else { continue };

            trace!(reply_event_id = ?event_item.identifier(), "Updating response to updated event");
            let in_reply_to = Some(InReplyToDetails {
                event_id: in_reply_to.event_id.clone(),
                event: TimelineDetails::Ready(Box::new(RepliedToEvent::from_timeline_item(
                    new_item,
                ))),
            });

            let new_reply_content = TimelineItemContent::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::Message(message.clone()),
                reactions: msglike.reactions.clone(),
                thread_root: msglike.thread_root.clone(),
                in_reply_to,
                thread_summary: msglike.thread_summary.clone(),
            });
            let new_reply_item = item.with_kind(event_item.with_content(new_reply_content));
            items.replace(timeline_item_index, new_reply_item);
        }
    }
}

/// Transfer `TimelineDetails` that weren't available on the original
/// item and have been fetched separately (only `reply_to` for
/// now) from `old_item` to `item`, given two items for an event
/// that was re-received.
///
/// `old_item` *should* always be a local timeline item usually, but it
/// can be a remote timeline item.
fn transfer_details(new_item: &mut EventTimelineItem, old_item: &EventTimelineItem) {
    let TimelineItemContent::MsgLike(new_msglike) = &mut new_item.content else {
        return;
    };
    let TimelineItemContent::MsgLike(old_msglike) = &old_item.content else {
        return;
    };

    let Some(in_reply_to) = &mut new_msglike.in_reply_to else { return };
    let Some(old_in_reply_to) = &old_msglike.in_reply_to else { return };

    if matches!(&in_reply_to.event, TimelineDetails::Unavailable) {
        in_reply_to.event = old_in_reply_to.event.clone();
    }
}
