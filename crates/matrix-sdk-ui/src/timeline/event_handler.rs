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

use std::{borrow::Cow, sync::Arc};

use as_variant::as_variant;
use indexmap::IndexMap;
use matrix_sdk::{
    deserialized_responses::{EncryptionInfo, UnableToDecryptInfo},
    send_queue::SendHandle,
};
use matrix_sdk_base::crypto::types::events::UtdCause;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    TransactionId,
    events::{
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, FullStateEventContent, MessageLikeEventContent, MessageLikeEventType,
        StateEventType, SyncStateEvent,
        poll::unstable_start::{
            NewUnstablePollStartEventContentWithoutRelation, UnstablePollStartEventContent,
        },
        receipt::Receipt,
        relation::Replacement,
        room::message::{
            Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
        },
    },
    serde::Raw,
};
use tracing::{debug, error, field::debug, instrument, trace, warn};

use super::{
    EmbeddedEvent, EncryptedMessage, EventTimelineItem, InReplyToDetails, MsgLikeContent,
    MsgLikeKind, OtherState, ReactionStatus, Sticker, ThreadSummary, TimelineDetails, TimelineItem,
    TimelineItemContent,
    controller::{
        Aggregation, AggregationKind, ObservableItemsTransaction, PendingEditKind,
        TimelineMetadata, TimelineStateTransaction, find_item_and_apply_aggregation,
    },
    date_dividers::DateDividerAdjuster,
    event_item::{
        AnyOtherFullStateEventContent, EventSendState, EventTimelineItemKind,
        LocalEventTimelineItem, PollState, Profile, RemoteEventOrigin, RemoteEventTimelineItem,
        TimelineEventItemId,
    },
    traits::RoomDataProvider,
};
use crate::{
    timeline::{controller::aggregations::PendingEdit, event_item::OtherMessageLike},
    unable_to_decrypt_hook::UtdHookManager,
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
        encryption_info: Option<Arc<EncryptionInfo>>,
    },
}

impl Flow {
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
    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, the user ID of the forwarder.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub(super) forwarder: Option<OwnedUserId>,
    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, the forwarder's profile.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub(super) forwarder_profile: Option<Profile>,
    /// The event's `origin_server_ts` field (or creation time for local echo).
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
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

/// Which kind of aggregation (i.e. modification of a related event) are we
/// going to handle?
#[derive(Clone, Debug)]
pub(super) enum HandleAggregationKind {
    /// Adding a reaction to the related event.
    Reaction { key: String },

    /// Redacting (removing) the related event.
    Redaction,

    /// Editing (replacing) the related event with another one.
    Edit { replacement: Replacement<RoomMessageEventContentWithoutRelation> },

    /// Responding to the related poll event.
    PollResponse { answers: Vec<String> },

    /// Editing a related poll event's description.
    PollEdit { replacement: Replacement<NewUnstablePollStartEventContentWithoutRelation> },

    /// Ending a related poll.
    PollEnd,
}

impl HandleAggregationKind {
    /// Returns a small string describing this aggregation, for debug purposes.
    pub fn debug_string(&self) -> &'static str {
        match self {
            HandleAggregationKind::Reaction { .. } => "a reaction",
            HandleAggregationKind::Redaction => "a redaction",
            HandleAggregationKind::Edit { .. } => "an edit",
            HandleAggregationKind::PollResponse { .. } => "a poll response",
            HandleAggregationKind::PollEdit { .. } => "a poll edit",
            HandleAggregationKind::PollEnd => "a poll end",
        }
    }
}

/// An action that we want to cause on the timeline.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum TimelineAction {
    /// Add a new timeline item.
    ///
    /// This enqueues adding a new item to the timeline (i.e. push to the items
    /// array in its state). The item may be filtered out, and thus not
    /// added later.
    AddItem {
        /// The content of the item we want to add.
        content: TimelineItemContent,
    },

    /// Handle an aggregation to another event.
    ///
    /// The event the aggregation is related to might not be included in the
    /// timeline, in which case it will be stashed somewhere, until we see
    /// the related event.
    HandleAggregation {
        /// To which other event does this aggregation apply to?
        related_event: OwnedEventId,
        /// What kind of aggregation are we handling here?
        kind: HandleAggregationKind,
    },
}

impl TimelineAction {
    /// Create a new [`TimelineEventKind::AddItem`].
    fn add_item(content: TimelineItemContent) -> Self {
        Self::AddItem { content }
    }

    /// Create a new [`TimelineAction`] from a given remote event.
    ///
    /// The return value may be `None` if the event was a redacted reaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn from_event<P: RoomDataProvider>(
        event: AnySyncTimelineEvent,
        raw_event: &Raw<AnySyncTimelineEvent>,
        room_data_provider: &P,
        unable_to_decrypt: Option<(UnableToDecryptInfo, Option<&Arc<UtdHookManager>>)>,
        in_reply_to: Option<InReplyToDetails>,
        thread_root: Option<OwnedEventId>,
        thread_summary: Option<ThreadSummary>,
    ) -> Option<Self> {
        let redaction_rules = room_data_provider.room_version_rules().redaction;

        let redacted_message_or_none = |event_type: MessageLikeEventType| {
            (event_type != MessageLikeEventType::Reaction)
                .then_some(TimelineItemContent::MsgLike(MsgLikeContent::redacted()))
        };

        Some(match event {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) => {
                if let Some(redacts) = ev.redacts(&redaction_rules).map(ToOwned::to_owned) {
                    Self::HandleAggregation {
                        related_event: redacts,
                        kind: HandleAggregationKind::Redaction,
                    }
                } else {
                    Self::add_item(redacted_message_or_none(ev.event_type())?)
                }
            }

            AnySyncTimelineEvent::MessageLike(ev) => match ev.original_content() {
                Some(AnyMessageLikeEventContent::RoomEncrypted(content)) => {
                    // An event which is still encrypted.
                    if let Some((unable_to_decrypt_info, unable_to_decrypt_hook_manager)) =
                        unable_to_decrypt
                    {
                        let utd_cause = UtdCause::determine(
                            raw_event,
                            room_data_provider.crypto_context_info().await,
                            &unable_to_decrypt_info,
                        );

                        // Let the hook know that we ran into an unable-to-decrypt that is added to
                        // the timeline.
                        if let Some(hook) = unable_to_decrypt_hook_manager {
                            hook.on_utd(
                                ev.event_id(),
                                utd_cause,
                                ev.origin_server_ts(),
                                ev.sender(),
                            )
                            .await;
                        }

                        Self::add_item(TimelineItemContent::MsgLike(
                            MsgLikeContent::unable_to_decrypt(EncryptedMessage::from_content(
                                content, utd_cause,
                            )),
                        ))
                    } else {
                        // If we get here, it means that some part of the code has created a
                        // `TimelineEvent` containing an `m.room.encrypted` event without
                        // decrypting it. Possibly this means that encryption has not been
                        // configured. We treat it the same as any other message-like event.
                        Self::from_content(
                            AnyMessageLikeEventContent::RoomEncrypted(content),
                            in_reply_to,
                            thread_root,
                            thread_summary,
                        )
                    }
                }

                Some(content) => {
                    Self::from_content(content, in_reply_to, thread_root, thread_summary)
                }

                None => Self::add_item(redacted_message_or_none(ev.event_type())?),
            },

            AnySyncTimelineEvent::State(ev) => match ev {
                AnySyncStateEvent::RoomMember(ev) => match ev {
                    SyncStateEvent::Original(ev) => {
                        Self::add_item(TimelineItemContent::room_member(
                            ev.state_key,
                            FullStateEventContent::Original {
                                content: ev.content,
                                prev_content: ev.unsigned.prev_content,
                            },
                            ev.sender,
                        ))
                    }
                    SyncStateEvent::Redacted(ev) => {
                        Self::add_item(TimelineItemContent::room_member(
                            ev.state_key,
                            FullStateEventContent::Redacted(ev.content),
                            ev.sender,
                        ))
                    }
                },
                ev => Self::add_item(TimelineItemContent::OtherState(OtherState {
                    state_key: ev.state_key().to_owned(),
                    content: AnyOtherFullStateEventContent::with_event_content(ev.content()),
                })),
            },
        })
    }

    /// Create a new [`TimelineAction`] from a given event's content.
    ///
    /// This is applicable to both remote event (as this is called from
    /// [`TimelineAction::from_event`]) or local events (for which we only have
    /// the content).
    ///
    /// The return value may be `None` if handling the event (be it a new item
    /// or an aggregation) is not supported for this event type.
    pub(super) fn from_content(
        content: AnyMessageLikeEventContent,
        in_reply_to: Option<InReplyToDetails>,
        thread_root: Option<OwnedEventId>,
        thread_summary: Option<ThreadSummary>,
    ) -> Self {
        match content {
            AnyMessageLikeEventContent::Reaction(c) => {
                // This is a reaction to a message.
                Self::HandleAggregation {
                    related_event: c.relates_to.event_id.clone(),
                    kind: HandleAggregationKind::Reaction { key: c.relates_to.key },
                }
            }

            AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent {
                relates_to: Some(Relation::Replacement(re)),
                ..
            }) => Self::HandleAggregation {
                related_event: re.event_id.clone(),
                kind: HandleAggregationKind::Edit { replacement: re },
            },

            AnyMessageLikeEventContent::UnstablePollStart(
                UnstablePollStartEventContent::Replacement(re),
            ) => Self::HandleAggregation {
                related_event: re.relates_to.event_id.clone(),
                kind: HandleAggregationKind::PollEdit { replacement: re.relates_to },
            },

            AnyMessageLikeEventContent::UnstablePollResponse(c) => Self::HandleAggregation {
                related_event: c.relates_to.event_id,
                kind: HandleAggregationKind::PollResponse { answers: c.poll_response.answers },
            },

            AnyMessageLikeEventContent::UnstablePollEnd(c) => Self::HandleAggregation {
                related_event: c.relates_to.event_id,
                kind: HandleAggregationKind::PollEnd,
            },

            AnyMessageLikeEventContent::CallInvite(_) => {
                Self::add_item(TimelineItemContent::CallInvite)
            }

            AnyMessageLikeEventContent::RtcNotification(_) => {
                Self::add_item(TimelineItemContent::RtcNotification)
            }

            AnyMessageLikeEventContent::Sticker(content) => {
                Self::add_item(TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Sticker(Sticker { content }),
                    reactions: Default::default(),
                    thread_root,
                    in_reply_to,
                    thread_summary,
                }))
            }

            AnyMessageLikeEventContent::UnstablePollStart(UnstablePollStartEventContent::New(
                c,
            )) => {
                let poll_state = PollState::new(c.poll_start, c.text);

                Self::AddItem {
                    content: TimelineItemContent::MsgLike(MsgLikeContent {
                        kind: MsgLikeKind::Poll(poll_state),
                        reactions: Default::default(),
                        thread_root,
                        in_reply_to,
                        thread_summary,
                    }),
                }
            }

            AnyMessageLikeEventContent::RoomMessage(msg) => Self::AddItem {
                content: TimelineItemContent::message(
                    msg.msgtype,
                    msg.mentions,
                    Default::default(),
                    thread_root,
                    in_reply_to,
                    thread_summary,
                ),
            },

            event => {
                let other = OtherMessageLike { event_type: event.event_type() };

                Self::AddItem {
                    content: TimelineItemContent::MsgLike(MsgLikeContent {
                        kind: MsgLikeKind::Other(other),
                        reactions: Default::default(),
                        thread_root,
                        in_reply_to,
                        thread_summary,
                    }),
                }
            }
        }
    }

    pub(super) fn failed_to_parse(event: FailedToParseEvent, error: serde_json::Error) -> Self {
        let error = Arc::new(error);
        match event {
            FailedToParseEvent::State { event_type, state_key } => {
                Self::add_item(TimelineItemContent::FailedToParseState {
                    event_type,
                    state_key,
                    error,
                })
            }
            FailedToParseEvent::MsgLike(event_type) => {
                Self::add_item(TimelineItemContent::FailedToParseMessageLike { event_type, error })
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum FailedToParseEvent {
    MsgLike(MessageLikeEventType),
    State { event_type: StateEventType, state_key: String },
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

/// Whether an item was removed or not.
pub(super) type RemovedItem = bool;

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
}

impl<'a, 'o> TimelineEventHandler<'a, 'o> {
    pub(super) fn new<P: RoomDataProvider>(
        state: &'a mut TimelineStateTransaction<'o, P>,
        ctx: TimelineEventContext,
    ) -> Self {
        let TimelineStateTransaction { items, meta, .. } = state;
        Self { items, meta, ctx }
    }

    /// Handle an event.
    ///
    /// Returns if an item was added to the timeline due to the new timeline
    /// action. Items might not be added to the timeline for various reasons,
    /// some common ones are if the item:
    ///     - Contains an unsupported event type.
    ///     - Is an edit or a redaction.
    ///     - Contains a local echo turning into a remote echo.
    ///     - Contains a message that is already in the timeline but was now
    ///       decrypted.
    ///
    /// `raw_event` is only needed to determine the cause of any UTDs,
    /// so if we know this is not a UTD it can be None.
    #[instrument(skip_all, fields(txn_id, event_id, position))]
    pub(super) async fn handle_event(
        mut self,
        date_divider_adjuster: &mut DateDividerAdjuster,
        timeline_action: TimelineAction,
    ) -> bool {
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
        }

        let mut added_item = false;

        match timeline_action {
            TimelineAction::AddItem { content } => {
                if self.ctx.should_add_new_items {
                    self.add_item(content);
                    added_item = true;
                }
            }

            TimelineAction::HandleAggregation { related_event, kind } => match kind {
                HandleAggregationKind::Reaction { key } => {
                    self.handle_reaction(related_event, key);
                }
                HandleAggregationKind::Redaction => {
                    self.handle_redaction(related_event);
                }
                HandleAggregationKind::Edit { replacement } => {
                    self.handle_edit(
                        replacement.event_id.clone(),
                        PendingEditKind::RoomMessage(replacement),
                    );
                }
                HandleAggregationKind::PollResponse { answers } => {
                    self.handle_poll_response(related_event, answers);
                }
                HandleAggregationKind::PollEdit { replacement } => {
                    self.handle_edit(
                        replacement.event_id.clone(),
                        PendingEditKind::Poll(replacement),
                    );
                }
                HandleAggregationKind::PollEnd => {
                    self.handle_poll_end(related_event);
                }
            },
        }

        added_item
    }

    #[instrument(skip(self, edit_kind))]
    fn handle_edit(&mut self, edited_event_id: OwnedEventId, edit_kind: PendingEditKind) {
        let target = TimelineEventItemId::EventId(edited_event_id.clone());

        let encryption_info =
            as_variant!(&self.ctx.flow, Flow::Remote { encryption_info, .. } => encryption_info.clone()).flatten();
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::Edit(PendingEdit {
                kind: edit_kind,
                edit_json: self.ctx.flow.raw_event().cloned(),
                encryption_info,
                bundled_item_owner: None,
            }),
        );

        self.meta.aggregations.add(target.clone(), aggregation.clone());

        if let Some(new_item) = find_item_and_apply_aggregation(
            &self.meta.aggregations,
            self.items,
            &target,
            aggregation,
            &self.meta.room_version_rules,
        ) {
            // Update all events that replied to this message with the edited content.
            Self::maybe_update_responses(
                self.meta,
                self.items,
                &edited_event_id,
                EmbeddedEvent::from_timeline_item(&new_item),
            );
        }
    }

    /// Apply a reaction to a *remote* event.
    ///
    /// Reactions to local events are applied in
    /// [`crate::timeline::TimelineController::handle_local_echo`].
    #[instrument(skip(self))]
    fn handle_reaction(&mut self, relates_to: OwnedEventId, reaction_key: String) {
        let target = TimelineEventItemId::EventId(relates_to);

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
                key: reaction_key,
                sender: self.ctx.sender.clone(),
                timestamp: self.ctx.timestamp,
                reaction_status,
            },
        );

        self.meta.aggregations.add(target.clone(), aggregation.clone());
        find_item_and_apply_aggregation(
            &self.meta.aggregations,
            self.items,
            &target,
            aggregation,
            &self.meta.room_version_rules,
        );
    }

    fn handle_poll_response(&mut self, poll_event_id: OwnedEventId, answers: Vec<String>) {
        let target = TimelineEventItemId::EventId(poll_event_id);
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::PollResponse {
                sender: self.ctx.sender.clone(),
                timestamp: self.ctx.timestamp,
                answers,
            },
        );
        self.meta.aggregations.add(target.clone(), aggregation.clone());
        find_item_and_apply_aggregation(
            &self.meta.aggregations,
            self.items,
            &target,
            aggregation,
            &self.meta.room_version_rules,
        );
    }

    fn handle_poll_end(&mut self, poll_event_id: OwnedEventId) {
        let target = TimelineEventItemId::EventId(poll_event_id);
        let aggregation = Aggregation::new(
            self.ctx.flow.timeline_item_id(),
            AggregationKind::PollEnd { end_date: self.ctx.timestamp },
        );
        self.meta.aggregations.add(target.clone(), aggregation.clone());
        find_item_and_apply_aggregation(
            &self.meta.aggregations,
            self.items,
            &target,
            aggregation,
            &self.meta.room_version_rules,
        );
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

        let target = TimelineEventItemId::EventId(redacted.clone());
        let aggregation =
            Aggregation::new(self.ctx.flow.timeline_item_id(), AggregationKind::Redaction);
        self.meta.aggregations.add(target.clone(), aggregation.clone());

        find_item_and_apply_aggregation(
            &self.meta.aggregations,
            self.items,
            &target,
            aggregation,
            &self.meta.room_version_rules,
        );

        // Even if the redacted event wasn't in the timeline, we can always update
        // responses with a placeholder "redacted" embedded item.
        let embedded_event = EmbeddedEvent {
            content: TimelineItemContent::MsgLike(MsgLikeContent::redacted()),
            sender: self.ctx.sender.clone(),
            sender_profile: TimelineDetails::from_initial_value(self.ctx.sender_profile.clone()),
            timestamp: self.ctx.timestamp,
            identifier: TimelineEventItemId::EventId(redacted.clone()),
        };

        Self::maybe_update_responses(self.meta, self.items, &redacted, embedded_event);
    }

    /// Attempts to redact an aggregation (e.g. a reaction, a poll response,
    /// etc.).
    ///
    /// Returns true if it's succeeded.
    #[instrument(skip_all, fields(redacts = ?aggregation_id))]
    fn handle_aggregation_redaction(&mut self, aggregation_id: OwnedEventId) -> bool {
        let aggregation_id = TimelineEventItemId::EventId(aggregation_id);

        match self.meta.aggregations.try_remove_aggregation(&aggregation_id, self.items) {
            Ok(val) => val,
            // This wasn't a known aggregation that was redacted.
            Err(err) => {
                warn!("error while attempting to remove aggregation: {err}");
                // It could find an aggregation but didn't properly unapply it.
                true
            }
        }
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
    fn add_item(&mut self, content: TimelineItemContent) {
        let sender = self.ctx.sender.to_owned();
        let sender_profile = TimelineDetails::from_initial_value(self.ctx.sender_profile.clone());

        let forwarder = self.ctx.forwarder.to_owned();
        let forwarder_profile = self
            .ctx
            .forwarder
            .as_ref()
            .map(|_| TimelineDetails::from_initial_value(self.ctx.forwarder_profile.clone()));

        let timestamp = self.ctx.timestamp;

        let kind: EventTimelineItemKind = match &self.ctx.flow {
            Flow::Local { txn_id, send_handle } => LocalEventTimelineItem {
                send_state: EventSendState::NotSentYet { progress: None },
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
                    is_own: self.ctx.sender == self.meta.own_user_id,
                    is_highlighted: self.ctx.is_highlighted,
                    encryption_info: encryption_info.clone(),
                    original_json: Some(raw_event.clone()),
                    latest_edit_json: None,
                    origin,
                }
                .into()
            }
        };

        let is_room_encrypted = self.meta.is_room_encrypted;

        let item = EventTimelineItem::new(
            sender,
            sender_profile,
            forwarder,
            forwarder_profile,
            timestamp,
            content,
            kind,
            is_room_encrypted,
        );

        // Apply any pending or stashed aggregations.
        let mut cowed = Cow::Owned(item);
        if let Err(err) = self.meta.aggregations.apply_all(
            &self.ctx.flow.timeline_item_id(),
            &mut cowed,
            self.items,
            &self.meta.room_version_rules,
        ) {
            warn!("discarding aggregations: {err}");
        }
        let item = cowed.into_owned();

        match &self.ctx.flow {
            Flow::Local { .. } => {
                trace!("Adding new local timeline item");

                let item = self.meta.new_timeline_item(item);

                self.items.push_local(item);
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
                        .iter_remotes_region()
                        .rev()
                        .find_map(|(timeline_item_index, timeline_item)| {
                            timeline_item.as_event().map(|_| timeline_item_index + 1)
                        })
                        .unwrap_or_else(|| {
                            // There is no remote timeline item, so we could insert at the start of
                            // the remotes region.
                            self.items.first_remotes_region_index()
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

                // Let's find the latest remote event and insert after it
                let timeline_item_index = self
                    .items
                    .iter_remotes_region()
                    .rev()
                    .find_map(|(timeline_item_index, timeline_item)| {
                        timeline_item.as_event().map(|_| timeline_item_index + 1)
                    })
                    .unwrap_or_else(|| {
                        // There is no remote timeline item, so we could insert at the start of
                        // the remotes region.
                        self.items.first_remotes_region_index()
                    });

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
                Self::maybe_update_responses(
                    self.meta,
                    self.items,
                    decrypted_event_id,
                    EmbeddedEvent::from_timeline_item(&item),
                );

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
            // Iterate the locals region.
            .iter_locals_region()
            // Iterate from the end to the start.
            .rev()
            .find_map(|(nth, timeline_item)| {
                let event_timeline_item = timeline_item.as_event()?;

                if Some(event_id) == event_timeline_item.event_id()
                    || (transaction_id.is_some()
                        && transaction_id == event_timeline_item.transaction_id())
                {
                    // A duplicate local event timeline item has been found!
                    Some((nth, event_timeline_item))
                } else {
                    // This local event timeline is not the one we are looking for. Continue our
                    // search.
                    None
                }
            })
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
        new_embedded_event: EmbeddedEvent,
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
            let in_reply_to = InReplyToDetails {
                event_id: in_reply_to.event_id.clone(),
                event: TimelineDetails::Ready(Box::new(new_embedded_event.clone())),
            };

            let new_reply_content = TimelineItemContent::MsgLike(
                msglike
                    .with_in_reply_to(in_reply_to)
                    .with_kind(MsgLikeKind::Message(message.clone())),
            );
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
