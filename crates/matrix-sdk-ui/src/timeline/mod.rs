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

//! A high-level view into a room's contents.
//!
//! See [`Timeline`] for details.

use std::{path::PathBuf, pin::Pin, sync::Arc, task::Poll};

use eyeball_im::VectorDiff;
use futures_core::Stream;
use imbl::Vector;
use matrix_sdk::{
    attachment::AttachmentConfig,
    event_cache::{EventCacheDropHandles, RoomEventCache},
    event_handler::EventHandlerHandle,
    executor::JoinHandle,
    room::{Receipts, Room},
    send_queue::{AbortSendHandle, RoomSendingQueueError},
    Client, Result,
};
use matrix_sdk_base::RoomState;
use mime::Mime;
use pin_project_lite::pin_project;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    events::{
        poll::unstable_start::{
            ReplacementUnstablePollStartEventContent, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread},
        relation::Annotation,
        room::{
            message::{
                AddMentions, ForwardThread, OriginalRoomMessageEvent, ReplacementMetadata,
                RoomMessageEventContentWithoutRelation,
            },
            redaction::RoomRedactionEventContent,
        },
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
    },
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, RoomVersionId,
    TransactionId, UserId,
};
use thiserror::Error;
use tracing::{error, instrument, trace, warn};

use self::{error::SendEventError, futures::SendAttachment};

mod builder;
mod day_dividers;
mod error;
mod event_handler;
mod event_item;
pub mod event_type_filter;
pub mod futures;
mod inner;
mod item;
mod pagination;
mod polls;
mod reactions;
mod read_receipts;
mod sliding_sync_ext;
#[cfg(test)]
mod tests;
#[cfg(feature = "e2e-encryption")]
mod to_device;
mod traits;
mod util;
mod virtual_item;

pub use self::{
    builder::TimelineBuilder,
    error::{Error, PaginationError, UnsupportedEditItem, UnsupportedReplyItem},
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, EventItemOrigin,
        EventSendState, EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange,
        Message, OtherState, Profile, ReactionGroup, RepliedToEvent, RoomMembershipChange, Sticker,
        TimelineDetails, TimelineItemContent,
    },
    event_type_filter::TimelineEventTypeFilter,
    inner::default_event_filter,
    item::{TimelineItem, TimelineItemKind},
    pagination::LiveBackPaginationStatus,
    polls::PollResult,
    reactions::ReactionSenderData,
    sliding_sync_ext::SlidingSyncRoomExt,
    traits::RoomExt,
    virtual_item::VirtualTimelineItem,
};
use self::{
    inner::{ReactionAction, TimelineInner},
    reactions::ReactionToggleResult,
    util::rfind_event_by_id,
};

/// A high-level view into a regular¹ room's contents.
///
/// ¹ This type is meant to be used in the context of rooms without a
/// `room_type`, that is rooms that are primarily used to exchange text
/// messages.
#[derive(Debug)]
pub struct Timeline {
    /// Clonable, inner fields of the `Timeline`, shared with some background
    /// tasks.
    inner: TimelineInner,

    /// The event cache specialized for this room's view.
    event_cache: RoomEventCache,

    /// References to long-running tasks held by the timeline.
    drop_handle: Arc<TimelineDropHandle>,
}

// Implements hash etc
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
struct AnnotationKey {
    event_id: OwnedEventId,
    key: String,
}

impl From<&Annotation> for AnnotationKey {
    fn from(annotation: &Annotation) -> Self {
        Self { event_id: annotation.event_id.clone(), key: annotation.key.clone() }
    }
}

/// What should the timeline focus on?
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineFocus {
    /// Focus on live events, i.e. receive events from sync and append them in
    /// real-time.
    Live,

    /// Focus on a specific event, e.g. after clicking a permalink.
    Event { target: OwnedEventId, num_context_events: u16 },
}

impl Timeline {
    /// Create a new [`TimelineBuilder`] for the given room.
    pub fn builder(room: &Room) -> TimelineBuilder {
        TimelineBuilder::new(room)
    }

    /// Returns the room for this timeline.
    pub fn room(&self) -> &Room {
        self.inner.room()
    }

    /// Clear all timeline items.
    pub async fn clear(&self) {
        self.inner.clear().await;
    }

    /// Retry decryption of previously un-decryptable events given a list of
    /// session IDs whose keys have been imported.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{Client, config::SyncSettings, ruma::room_id};
    /// # use matrix_sdk_ui::Timeline;
    /// # async {
    /// # let mut client: Client = todo!();
    /// # let room_id = ruma::room_id!("!example:example.org");
    /// # let timeline: Timeline = todo!();
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// let result =
    ///     client.encryption().import_room_keys(path, "secret-passphrase").await?;
    ///
    /// // Given a timeline for a specific room_id
    /// if let Some(keys_for_users) = result.keys.get(room_id) {
    ///     let session_ids = keys_for_users.values().flatten();
    ///     timeline.retry_decryption(session_ids).await;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(feature = "e2e-encryption")]
    pub async fn retry_decryption<S: Into<String>>(
        &self,
        session_ids: impl IntoIterator<Item = S>,
    ) {
        self.inner
            .retry_event_decryption(
                self.room(),
                Some(session_ids.into_iter().map(Into::into).collect()),
            )
            .await;
    }

    #[cfg(feature = "e2e-encryption")]
    #[tracing::instrument(skip(self))]
    async fn retry_decryption_for_all_events(&self) {
        self.inner.retry_event_decryption(self.room(), None).await;
    }

    /// Get the current timeline item for the given event ID, if any.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn item_by_event_id(&self, event_id: &EventId) -> Option<EventTimelineItem> {
        let items = self.inner.items().await;
        let (_, item) = rfind_event_by_id(&items, event_id)?;
        Some(item.to_owned())
    }

    /// Get the latest of the timeline's event items.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        if self.inner.is_live().await {
            self.inner.items().await.last()?.as_event().cloned()
        } else {
            None
        }
    }

    /// Get the current timeline items, and a stream of changes.
    ///
    /// You can poll this stream to receive updates. See
    /// [`futures_util::StreamExt`] for a high-level API on top of [`Stream`].
    pub async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = VectorDiff<Arc<TimelineItem>>>) {
        let (items, stream) = self.inner.subscribe().await;
        let stream = TimelineStream::new(stream, self.drop_handle.clone());
        (items, stream)
    }

    /// Get the current timeline items, and a batched stream of changes.
    ///
    /// In contrast to [`subscribe`](Self::subscribe), this stream can yield
    /// multiple diffs at once. The batching is done such that no arbitrary
    /// delays are added.
    pub async fn subscribe_batched(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>) {
        let (items, stream) = self.inner.subscribe_batched().await;
        let stream = TimelineStream::new(stream, self.drop_handle.clone());
        (items, stream)
    }

    /// Send a message to the room, and add it to the timeline as a local echo.
    ///
    /// For simplicity, this method doesn't currently allow custom message
    /// types.
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// If sending the message fails, the local echo item will change its
    /// `send_state` to [`EventSendState::SendingFailed`].
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    ///
    /// [`MessageLikeUnsigned`]: ruma::events::MessageLikeUnsigned
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    #[instrument(skip(self, content), fields(room_id = ?self.room().room_id()))]
    pub async fn send(
        &self,
        content: AnyMessageLikeEventContent,
    ) -> Result<AbortSendHandle, RoomSendingQueueError> {
        self.room().sending_queue().send(content).await
    }

    /// Send a reply to the given event.
    ///
    /// Currently it only supports events with an event ID and JSON being
    /// available (which can be removed by local redactions). This is subject to
    /// change. Please check [`EventTimelineItem::can_be_replied_to`] to decide
    /// whether to render a reply button.
    ///
    /// The sender of `reply_item` will be added to the mentions of the reply if
    /// and only if `reply_item` has not been written by the sender.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the reply
    ///
    /// * `reply_item` - The event item you want to reply to
    ///
    /// * `forward_thread` - Usually `Yes`, unless you explicitly want to the
    ///   reply to show up in the main timeline even though the `reply_item` is
    ///   part of a thread
    #[instrument(skip(self, content, reply_item))]
    pub async fn send_reply(
        &self,
        content: RoomMessageEventContentWithoutRelation,
        reply_item: &EventTimelineItem,
        forward_thread: ForwardThread,
    ) -> Result<(), SendEventError> {
        // Error returns here must be in sync with
        // `EventTimelineItem::can_be_replied_to`
        let Some(event_id) = reply_item.event_id() else {
            return Err(UnsupportedReplyItem::MISSING_EVENT_ID.into());
        };

        // [The specification](https://spec.matrix.org/v1.10/client-server-api/#user-and-room-mentions) says:
        //
        // > Users should not add their own Matrix ID to the `m.mentions` property as
        // > outgoing messages cannot self-notify.
        //
        // If `reply_item` has been written by the current user, let's toggle to
        // `AddMentions::No`.
        let mention_the_sender = if self.room().own_user_id() == reply_item.sender {
            AddMentions::No
        } else {
            AddMentions::Yes
        };

        let content = match reply_item.content() {
            TimelineItemContent::Message(msg) => {
                let event = OriginalRoomMessageEvent {
                    event_id: event_id.to_owned(),
                    sender: reply_item.sender().to_owned(),
                    origin_server_ts: reply_item.timestamp(),
                    room_id: self.room().room_id().to_owned(),
                    content: msg.to_content(),
                    unsigned: Default::default(),
                };
                content.make_reply_to(&event, forward_thread, mention_the_sender)
            }
            _ => {
                let Some(raw_event) = reply_item.latest_json() else {
                    return Err(UnsupportedReplyItem::MISSING_JSON.into());
                };

                content.make_reply_to_raw(
                    raw_event,
                    event_id.to_owned(),
                    self.room().room_id(),
                    forward_thread,
                    mention_the_sender,
                )
            }
        };

        self.send(content.into()).await?;

        Ok(())
    }

    /// Send an edit to the given event.
    ///
    /// Currently only supports `m.room.message` events whose event ID is known.
    /// Please check [`EventTimelineItem::can_be_edited`] before calling this.
    ///
    /// # Arguments
    ///
    /// * `new_content` - The content of the reply
    ///
    /// * `edit_item` - The event item you want to edit
    #[instrument(skip(self, new_content))]
    pub async fn edit(
        &self,
        new_content: RoomMessageEventContentWithoutRelation,
        edit_item: &EventTimelineItem,
    ) -> Result<(), SendEventError> {
        // Early returns here must be in sync with
        // `EventTimelineItem::can_be_edited`
        let Some(event_id) = edit_item.event_id() else {
            return Err(UnsupportedEditItem::MISSING_EVENT_ID.into());
        };
        let TimelineItemContent::Message(original_content) = edit_item.content() else {
            return Err(UnsupportedEditItem::NOT_ROOM_MESSAGE.into());
        };

        let replied_to_message =
            original_content.in_reply_to().and_then(|details| match &details.event {
                TimelineDetails::Ready(event) => match event.content() {
                    TimelineItemContent::Message(msg) => Some(OriginalRoomMessageEvent {
                        content: msg.to_content(),
                        event_id: event_id.to_owned(),
                        sender: event.sender.clone(),
                        // Dummy value, not used by make_replacement
                        origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                        room_id: self.room().room_id().to_owned(),
                        unsigned: Default::default(),
                    }),
                    _ => None,
                },
                _ => {
                    warn!("original event is a reply, but we don't have the replied-to event");
                    None
                }
            });

        let content = new_content.make_replacement(
            ReplacementMetadata::new(event_id.to_owned(), original_content.mentions.clone()),
            replied_to_message.as_ref(),
        );

        self.send(content.into()).await?;

        Ok(())
    }

    pub async fn edit_poll(
        &self,
        fallback_text: impl Into<String>,
        poll: UnstablePollStartContentBlock,
        edit_item: &EventTimelineItem,
    ) -> Result<(), SendEventError> {
        let Some(event_id) = edit_item.event_id() else {
            return Err(UnsupportedEditItem::MISSING_EVENT_ID.into());
        };
        let TimelineItemContent::Poll(_) = edit_item.content() else {
            return Err(UnsupportedEditItem::NOT_POLL_EVENT.into());
        };

        let replacement_poll = ReplacementUnstablePollStartEventContent::plain_text(
            fallback_text,
            poll,
            event_id.into(),
        );
        self.send(UnstablePollStartEventContent::from(replacement_poll).into()).await?;
        Ok(())
    }

    /// Toggle a reaction on an event
    ///
    /// Adds or redacts a reaction based on the state of the reaction at the
    /// time it is called.
    ///
    /// When redacting an event, the redaction reason is not sent.
    ///
    /// Ensures that only one reaction is sent at a time to avoid race
    /// conditions and spamming the homeserver with requests.
    pub async fn toggle_reaction(&self, annotation: &Annotation) -> Result<(), Error> {
        // Always toggle the local reaction immediately
        let mut action = self.inner.toggle_reaction_local(annotation).await?;

        // The local echo may have been updated while a reaction is in flight
        // so until it matches the state of the server, keep reconciling
        loop {
            let response = match action {
                ReactionAction::None => {
                    // The remote reaction matches the local reaction, OR
                    // there is already a request in flight which will resolve
                    // later, so stop here.
                    break;
                }
                ReactionAction::SendRemote(txn_id) => {
                    self.send_reaction(annotation, txn_id.to_owned()).await
                }
                ReactionAction::RedactRemote(event_id) => {
                    self.redact_reaction(&event_id.to_owned()).await
                }
            };

            action = self.inner.resolve_reaction_response(annotation, &response).await?;
        }
        Ok(())
    }

    /// Redact a reaction event from the homeserver
    async fn redact_reaction(&self, event_id: &EventId) -> ReactionToggleResult {
        let room = self.room();
        if room.state() != RoomState::Joined {
            warn!("Cannot redact a reaction in a room that is not joined");
            return ReactionToggleResult::RedactFailure { event_id: event_id.to_owned() };
        }

        let txn_id = TransactionId::new();
        let no_reason = RoomRedactionEventContent::default();

        let response = room.redact(event_id, no_reason.reason.as_deref(), Some(txn_id)).await;

        match response {
            Ok(_) => ReactionToggleResult::RedactSuccess,
            Err(error) => {
                error!("Failed to redact reaction: {error}");
                ReactionToggleResult::RedactFailure { event_id: event_id.to_owned() }
            }
        }
    }

    /// Send a reaction event to the homeserver
    async fn send_reaction(
        &self,
        annotation: &Annotation,
        txn_id: OwnedTransactionId,
    ) -> ReactionToggleResult {
        let room = self.room();
        if room.state() != RoomState::Joined {
            warn!("Cannot send a reaction in a room that is not joined");
            return ReactionToggleResult::AddFailure { txn_id };
        }

        let event_content =
            AnyMessageLikeEventContent::Reaction(ReactionEventContent::from(annotation.clone()));
        let response = room.send(event_content).with_transaction_id(&txn_id).await;

        match response {
            Ok(response) => {
                ReactionToggleResult::AddSuccess { event_id: response.event_id, txn_id }
            }
            Err(error) => {
                error!("Failed to send reaction: {error}");
                ReactionToggleResult::AddFailure { txn_id }
            }
        }
    }

    /// Sends an attachment to the room. It does not currently support local
    /// echoes
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file to be sent
    ///
    /// * `mime_type` - The attachment's mime type
    ///
    /// * `config` - An attachment configuration object containing details about
    ///   the attachment
    ///
    /// like a thumbnail, its size, duration etc.
    #[instrument(skip_all)]
    pub fn send_attachment(
        &self,
        path: impl Into<PathBuf>,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> SendAttachment<'_> {
        SendAttachment::new(self, path.into(), mime_type, config)
    }

    /// Fetch unavailable details about the event with the given ID.
    ///
    /// This method only works for IDs of remote [`EventTimelineItem`]s,
    /// to prevent losing details when a local echo is replaced by its
    /// remote echo.
    ///
    /// This method tries to make all the requests it can. If an error is
    /// encountered for a given request, it is forwarded with the
    /// [`TimelineDetails::Error`] variant.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The event ID of the event to fetch details for.
    ///
    /// # Errors
    ///
    /// Returns an error if the identifier doesn't match any event with a remote
    /// echo in the timeline, or if the event is removed from the timeline
    /// before all requests are handled.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn fetch_details_for_event(&self, event_id: &EventId) -> Result<(), Error> {
        self.inner.fetch_in_reply_to_details(event_id).await
    }

    /// Fetch all member events for the room this timeline is displaying.
    ///
    /// If the full member list is not known, sender profiles are currently
    /// likely not going to be available. This will be fixed in the future.
    ///
    /// If fetching the members fails, any affected timeline items will have
    /// the `sender_profile` set to [`TimelineDetails::Error`].
    #[instrument(skip_all)]
    pub async fn fetch_members(&self) {
        self.inner.set_sender_profiles_pending().await;
        match self.room().sync_members().await {
            Ok(_) => {
                self.inner.update_missing_sender_profiles().await;
            }
            Err(e) => {
                self.inner.set_sender_profiles_error(Arc::new(e)).await;
            }
        }
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Contrary to [`Room::load_user_receipt()`] that only keeps track of read
    /// receipts received from the homeserver, this keeps also track of implicit
    /// read receipts in this timeline, i.e. when a room member sends an event.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.inner.latest_user_read_receipt(user_id).await
    }

    /// Get the ID of the timeline event with the latest read receipt for the
    /// given user.
    ///
    /// In contrary to [`Self::latest_user_read_receipt()`], this allows to know
    /// the position of the read receipt in the timeline even if the event it
    /// applies to is not visible in the timeline, unless the event is unknown
    /// by this timeline.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        self.inner.latest_user_read_receipt_timeline_event_id(user_id).await
    }

    /// Send the given receipt.
    ///
    /// This uses [`Room::send_single_receipt`] internally, but checks
    /// first if the receipt points to an event in this timeline that is more
    /// recent than the current ones, to avoid unnecessary requests.
    ///
    /// Returns a boolean indicating if it sent the request or not.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn send_single_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: OwnedEventId,
    ) -> Result<bool> {
        if !self.inner.should_send_receipt(&receipt_type, &thread, &event_id).await {
            trace!(
                "not sending receipt, because we already cover the event with a previous receipt"
            );
            return Ok(false);
        }

        trace!("sending receipt");
        self.room().send_single_receipt(receipt_type, thread, event_id).await?;
        Ok(true)
    }

    /// Send the given receipts.
    ///
    /// This uses [`Room::send_multiple_receipts`] internally, but
    /// checks first if the receipts point to events in this timeline that
    /// are more recent than the current ones, to avoid unnecessary
    /// requests.
    #[instrument(skip(self))]
    pub async fn send_multiple_receipts(&self, mut receipts: Receipts) -> Result<()> {
        if let Some(fully_read) = &receipts.fully_read {
            if !self
                .inner
                .should_send_receipt(
                    &ReceiptType::FullyRead,
                    &ReceiptThread::Unthreaded,
                    fully_read,
                )
                .await
            {
                receipts.fully_read = None;
            }
        }

        if let Some(read_receipt) = &receipts.public_read_receipt {
            if !self
                .inner
                .should_send_receipt(&ReceiptType::Read, &ReceiptThread::Unthreaded, read_receipt)
                .await
            {
                receipts.public_read_receipt = None;
            }
        }

        if let Some(private_read_receipt) = &receipts.private_read_receipt {
            if !self
                .inner
                .should_send_receipt(
                    &ReceiptType::ReadPrivate,
                    &ReceiptThread::Unthreaded,
                    private_read_receipt,
                )
                .await
            {
                receipts.private_read_receipt = None;
            }
        }

        self.room().send_multiple_receipts(receipts).await
    }

    /// Mark the room as read by sending an unthreaded read receipt on the
    /// latest event, be it visible or not.
    ///
    /// This works even if the latest event belongs to a thread, as a threaded
    /// reply also belongs to the unthreaded timeline. No threaded receipt
    /// will be sent here (see also #3123).
    ///
    /// Returns a boolean indicating if we sent the request or not.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn mark_as_read(&self, receipt_type: ReceiptType) -> Result<bool> {
        if let Some(event_id) = self.inner.latest_event_id().await {
            self.send_single_receipt(receipt_type, ReceiptThread::Unthreaded, event_id).await
        } else {
            trace!("can't mark room as read because there's no latest event id");
            Ok(false)
        }
    }
}

/// Test helpers, likely not very useful in production.
#[doc(hidden)]
impl Timeline {
    /// Get the current list of timeline items.
    pub async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.inner.items().await
    }

    pub async fn subscribe_filter_map<U: Clone>(
        &self,
        f: impl Fn(Arc<TimelineItem>) -> Option<U>,
    ) -> (Vector<U>, impl Stream<Item = VectorDiff<U>>) {
        let (items, stream) = self.inner.subscribe_filter_map(f).await;
        let stream = TimelineStream::new(stream, self.drop_handle.clone());
        (items, stream)
    }
}

#[derive(Debug)]
struct TimelineDropHandle {
    client: Client,
    event_handler_handles: Vec<EventHandlerHandle>,
    room_update_join_handle: JoinHandle<()>,
    room_key_from_backups_join_handle: JoinHandle<()>,
    local_echo_listener_handle: JoinHandle<()>,
    _event_cache_drop_handle: Arc<EventCacheDropHandles>,
}

impl Drop for TimelineDropHandle {
    fn drop(&mut self) {
        for handle in self.event_handler_handles.drain(..) {
            self.client.remove_event_handler(handle);
        }
        self.local_echo_listener_handle.abort();
        self.room_update_join_handle.abort();
        self.room_key_from_backups_join_handle.abort();
    }
}

pin_project! {
    struct TimelineStream<S> {
        #[pin]
        inner: S,
        drop_handle: Arc<TimelineDropHandle>,
    }
}

impl<S> TimelineStream<S> {
    fn new(inner: S, drop_handle: Arc<TimelineDropHandle>) -> Self {
        Self { inner, drop_handle }
    }
}

impl<S: Stream> Stream for TimelineStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

pub type TimelineEventFilterFn =
    dyn Fn(&AnySyncTimelineEvent, &RoomVersionId) -> bool + Send + Sync;
