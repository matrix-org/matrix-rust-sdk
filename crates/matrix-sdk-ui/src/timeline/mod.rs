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

use event_item::{extract_room_msg_edit_content, EventTimelineItemKind, TimelineItemHandle};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use imbl::Vector;
use matrix_sdk::{
    attachment::AttachmentConfig,
    event_cache::{EventCacheDropHandles, RoomEventCache},
    event_handler::EventHandlerHandle,
    executor::JoinHandle,
    room::{edit::EditedContent, Receipts, Room},
    send_queue::{RoomSendQueueError, SendHandle},
    Client, Result,
};
use mime::Mime;
use pin_project_lite::pin_project;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    events::{
        poll::unstable_start::{NewUnstablePollStartEventContent, UnstablePollStartEventContent},
        receipt::{Receipt, ReceiptThread},
        room::{
            message::{
                AddMentions, ForwardThread, OriginalRoomMessageEvent,
                RoomMessageEventContentWithoutRelation,
            },
            pinned_events::RoomPinnedEventsEventContent,
        },
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        SyncMessageLikeEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, RoomVersionId, TransactionId,
    UserId,
};
use thiserror::Error;
use tracing::{error, instrument, trace, warn};

use crate::timeline::pinned_events_loader::PinnedEventsRoom;

mod builder;
mod controller;
mod day_dividers;
mod error;
mod event_handler;
mod event_item;
pub mod event_type_filter;
pub mod futures;
mod item;
mod pagination;
mod pinned_events_loader;
mod reactions;
mod read_receipts;
#[cfg(test)]
mod tests;
mod to_device;
mod traits;
mod util;
mod virtual_item;

pub use self::{
    builder::TimelineBuilder,
    controller::default_event_filter,
    error::*,
    event_item::{
        AnyOtherFullStateEventContent, EncryptedMessage, EventItemOrigin, EventSendState,
        EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange, Message,
        OtherState, PollResult, PollState, Profile, ReactionInfo, ReactionStatus,
        ReactionsByKeyBySender, RepliedToEvent, RoomMembershipChange, RoomPinnedEventsChange,
        Sticker, TimelineDetails, TimelineEventItemId, TimelineItemContent,
    },
    event_type_filter::TimelineEventTypeFilter,
    item::{TimelineItem, TimelineItemKind},
    pagination::LiveBackPaginationStatus,
    traits::RoomExt,
    virtual_item::VirtualTimelineItem,
};
use self::{
    controller::TimelineController,
    futures::SendAttachment,
    util::{rfind_event_by_id, rfind_event_item},
};

/// Information needed to reply to an event.
#[derive(Debug, Clone)]
pub struct RepliedToInfo {
    /// The event ID of the event to reply to.
    event_id: OwnedEventId,
    /// The sender of the event to reply to.
    sender: OwnedUserId,
    /// The timestamp of the event to reply to.
    timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event to reply to.
    content: ReplyContent,
}

impl RepliedToInfo {
    /// The event ID of the event to reply to.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    /// The sender of the event to reply to.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// The content of the event to reply to.
    pub fn content(&self) -> &ReplyContent {
        &self.content
    }
}

/// The content of a reply.
#[derive(Debug, Clone)]
pub enum ReplyContent {
    /// Content of a message event.
    Message(Message),
    /// Content of any other kind of event stored as raw JSON.
    Raw(Raw<AnySyncTimelineEvent>),
}

/// A high-level view into a regular¹ room's contents.
///
/// ¹ This type is meant to be used in the context of rooms without a
/// `room_type`, that is rooms that are primarily used to exchange text
/// messages.
#[derive(Debug)]
pub struct Timeline {
    /// Cloneable, inner fields of the `Timeline`, shared with some background
    /// tasks.
    controller: TimelineController,

    /// The event cache specialized for this room's view.
    event_cache: RoomEventCache,

    /// References to long-running tasks held by the timeline.
    drop_handle: Arc<TimelineDropHandle>,
}

/// What should the timeline focus on?
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineFocus {
    /// Focus on live events, i.e. receive events from sync and append them in
    /// real-time.
    Live,

    /// Focus on a specific event, e.g. after clicking a permalink.
    Event { target: OwnedEventId, num_context_events: u16 },

    /// Only show pinned events.
    PinnedEvents { max_events_to_load: u16, max_concurrent_requests: u16 },
}

impl Timeline {
    /// Create a new [`TimelineBuilder`] for the given room.
    pub fn builder(room: &Room) -> TimelineBuilder {
        TimelineBuilder::new(room)
    }

    /// Returns the room for this timeline.
    pub fn room(&self) -> &Room {
        self.controller.room()
    }

    /// Clear all timeline items.
    pub async fn clear(&self) {
        self.controller.clear().await;
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
    pub async fn retry_decryption<S: Into<String>>(
        &self,
        session_ids: impl IntoIterator<Item = S>,
    ) {
        self.controller
            .retry_event_decryption(
                self.room(),
                Some(session_ids.into_iter().map(Into::into).collect()),
            )
            .await;
    }

    #[tracing::instrument(skip(self))]
    async fn retry_decryption_for_all_events(&self) {
        self.controller.retry_event_decryption(self.room(), None).await;
    }

    /// Get the current timeline item for the given [`TimelineEventItemId`], if
    /// any.
    async fn event_by_timeline_id(&self, id: &TimelineEventItemId) -> Option<EventTimelineItem> {
        match id {
            TimelineEventItemId::EventId(event_id) => self.item_by_event_id(event_id).await,
            TimelineEventItemId::TransactionId(transaction_id) => {
                self.item_by_transaction_id(transaction_id).await
            }
        }
    }

    /// Get the current timeline item for the given event ID, if any.
    ///
    /// Will return a remote event, *or* a local echo that has been sent but not
    /// yet replaced by a remote echo.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn item_by_event_id(&self, event_id: &EventId) -> Option<EventTimelineItem> {
        let items = self.controller.items().await;
        let (_, item) = rfind_event_by_id(&items, event_id)?;
        Some(item.to_owned())
    }

    /// Get the current local echo timeline item for the given transaction ID,
    /// if any.
    ///
    /// This will always return a local echo, if found.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn local_item_by_transaction_id(
        &self,
        target: &TransactionId,
    ) -> Option<EventTimelineItem> {
        let items = self.controller.items().await;
        let (_, item) = rfind_event_item(&items, |item| {
            item.as_local().map_or(false, |local| local.transaction_id == target)
        })?;
        Some(item.to_owned())
    }

    /// Get the latest of the timeline's event items.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        if self.controller.is_live().await {
            self.controller.items().await.last()?.as_event().cloned()
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
        let (items, stream) = self.controller.subscribe().await;
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
        let (items, stream) = self.controller.subscribe_batched().await;
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
    ) -> Result<SendHandle, RoomSendQueueError> {
        self.room().send_queue().send(content).await
    }

    /// Send a reply to the given event.
    ///
    /// Currently it only supports events with an event ID and JSON being
    /// available (which can be removed by local redactions). This is subject to
    /// change. Please check [`EventTimelineItem::can_be_replied_to`] to decide
    /// whether to render a reply button.
    ///
    /// The sender will be added to the mentions of the reply if
    /// and only if the event has not been written by the sender.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the reply
    ///
    /// * `replied_to_info` - A wrapper that contains the event ID, sender,
    ///   content and timestamp of the event to reply to
    ///
    /// * `forward_thread` - Usually `Yes`, unless you explicitly want to the
    ///   reply to show up in the main timeline even though the `reply_item` is
    ///   part of a thread
    #[instrument(skip(self, content, replied_to_info))]
    pub async fn send_reply(
        &self,
        content: RoomMessageEventContentWithoutRelation,
        replied_to_info: RepliedToInfo,
        forward_thread: ForwardThread,
    ) -> Result<(), RoomSendQueueError> {
        let event_id = replied_to_info.event_id;

        // [The specification](https://spec.matrix.org/v1.10/client-server-api/#user-and-room-mentions) says:
        //
        // > Users should not add their own Matrix ID to the `m.mentions` property as
        // > outgoing messages cannot self-notify.
        //
        // If the replied to event has been written by the current user, let's toggle to
        // `AddMentions::No`.
        let mention_the_sender = if self.room().own_user_id() == replied_to_info.sender {
            AddMentions::No
        } else {
            AddMentions::Yes
        };

        let content = match replied_to_info.content {
            ReplyContent::Message(msg) => {
                let event = OriginalRoomMessageEvent {
                    event_id: event_id.to_owned(),
                    sender: replied_to_info.sender,
                    origin_server_ts: replied_to_info.timestamp,
                    room_id: self.room().room_id().to_owned(),
                    content: msg.to_content(),
                    unsigned: Default::default(),
                };
                content.make_reply_to(&event, forward_thread, mention_the_sender)
            }
            ReplyContent::Raw(raw_event) => content.make_reply_to_raw(
                &raw_event,
                event_id.to_owned(),
                self.room().room_id(),
                forward_thread,
                mention_the_sender,
            ),
        };

        self.send(content.into()).await?;

        Ok(())
    }

    /// Get the information needed to reply to the event with the given ID.
    pub async fn replied_to_info_from_event_id(
        &self,
        event_id: &EventId,
    ) -> Result<RepliedToInfo, UnsupportedReplyItem> {
        if let Some(timeline_item) = self.item_by_event_id(event_id).await {
            return timeline_item.replied_to_info();
        }

        let event = self.room().event(event_id, None).await.map_err(|error| {
            error!("Failed to fetch event with ID {event_id} with error: {error}");
            UnsupportedReplyItem::MissingEvent
        })?;

        let raw_sync_event = event.into_raw();
        let sync_event = raw_sync_event.deserialize().map_err(|error| {
            error!("Failed to deserialize event with ID {event_id} with error: {error}");
            UnsupportedReplyItem::FailedToDeserializeEvent
        })?;

        let reply_content = match &sync_event {
            AnySyncTimelineEvent::MessageLike(message_like_event) => {
                if let AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(
                    original_message,
                )) = message_like_event
                {
                    ReplyContent::Message(Message::from_event(
                        original_message.content.clone(),
                        extract_room_msg_edit_content(message_like_event.relations()),
                        &self.items().await,
                    ))
                } else {
                    ReplyContent::Raw(raw_sync_event)
                }
            }
            AnySyncTimelineEvent::State(_) => return Err(UnsupportedReplyItem::StateEvent),
        };

        Ok(RepliedToInfo {
            event_id: event_id.to_owned(),
            sender: sync_event.sender().to_owned(),
            timestamp: sync_event.origin_server_ts(),
            content: reply_content,
        })
    }

    /// Returns a local or remote timeline item identified by this transaction
    /// id.
    async fn item_by_transaction_id(&self, txn_id: &TransactionId) -> Option<EventTimelineItem> {
        let items = self.controller.items().await;

        let (_, found) = rfind_event_item(&items, |item| match &item.kind {
            EventTimelineItemKind::Local(local) => local.transaction_id == txn_id,
            EventTimelineItemKind::Remote(remote) => {
                remote.transaction_id.as_deref() == Some(txn_id)
            }
        })?;

        Some(found.clone())
    }

    /// Edit an event given its [`TimelineEventItemId`] and some new content.
    ///
    /// See [`Self::edit`] for more information.
    pub async fn edit_by_id(
        &self,
        id: &TimelineEventItemId,
        new_content: EditedContent,
    ) -> Result<bool, Error> {
        let Some(event) = self.event_by_timeline_id(id).await else {
            return Err(Error::EventNotInTimeline(id.clone()));
        };

        self.edit(&event, new_content).await
    }

    /// Edit an event.
    ///
    /// Only supports events for which [`EventTimelineItem::is_editable()`]
    /// returns `true`.
    ///
    /// # Returns
    ///
    /// - Returns `Ok(true)` if the edit was added to the send queue.
    /// - Returns `Ok(false)` if the edit targets an item that has no local nor
    ///   matching remote item.
    /// - Returns an error if there was an issue sending the redaction event, or
    ///   interacting with the sending queue.
    #[instrument(skip(self, new_content))]
    pub async fn edit(
        &self,
        item: &EventTimelineItem,
        new_content: EditedContent,
    ) -> Result<bool, Error> {
        let event_id = match item.identifier() {
            TimelineEventItemId::TransactionId(txn_id) => {
                // See if we have an up-to-date timeline item with that transaction id.
                if let Some(item) = self.item_by_transaction_id(&txn_id).await {
                    match item.handle() {
                        TimelineItemHandle::Remote(event_id) => event_id.to_owned(),
                        TimelineItemHandle::Local(handle) => {
                            // Relations are filled by the editing code itself.
                            let new_content: AnyMessageLikeEventContent = match new_content {
                                EditedContent::RoomMessage(message) => {
                                    if matches!(item.content, TimelineItemContent::Message(_)) {
                                        AnyMessageLikeEventContent::RoomMessage(message.into())
                                    } else {
                                        warn!("New content (m.room.message) doesn't match previous event content.");
                                        return Ok(false);
                                    }
                                }
                                EditedContent::PollStart { new_content, .. } => {
                                    if matches!(item.content, TimelineItemContent::Poll(_)) {
                                        AnyMessageLikeEventContent::UnstablePollStart(
                                            UnstablePollStartEventContent::New(
                                                NewUnstablePollStartEventContent::new(new_content),
                                            ),
                                        )
                                    } else {
                                        warn!("New content (poll start) doesn't match previous event content.");
                                        return Ok(false);
                                    }
                                }
                            };
                            return Ok(handle
                                .edit(new_content)
                                .await
                                .map_err(RoomSendQueueError::StorageError)?);
                        }
                    }
                } else {
                    warn!("Couldn't find the local echo anymore, nor a matching remote echo");
                    return Ok(false);
                }
            }

            TimelineEventItemId::EventId(event_id) => event_id,
        };

        let content = self.room().make_edit_event(&event_id, new_content).await?;

        self.send(content).await?;

        Ok(true)
    }

    /// Toggle a reaction on an event.
    ///
    /// The `unique_id` parameter is a string returned by
    /// [`TimelineItem::unique_id()`].
    ///
    /// Adds or redacts a reaction based on the state of the reaction at the
    /// time it is called.
    ///
    /// When redacting a previous reaction, the redaction reason is not set.
    ///
    /// Ensures that only one reaction is sent at a time to avoid race
    /// conditions and spamming the homeserver with requests.
    pub async fn toggle_reaction(&self, unique_id: &str, reaction_key: &str) -> Result<(), Error> {
        self.controller.toggle_reaction_local(unique_id, reaction_key).await?;
        Ok(())
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

    /// Redact an event given its [`TimelineEventItemId`] and an optional
    /// reason.
    ///
    /// See [`Self::redact`] for more info.
    pub async fn redact_by_id(
        &self,
        id: &TimelineEventItemId,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let event_id = match id {
            TimelineEventItemId::TransactionId(transaction_id) => {
                let Some(item) = self.item_by_transaction_id(transaction_id).await else {
                    return Err(Error::RedactError(RedactError::LocalEventNotFound(
                        transaction_id.to_owned(),
                    )));
                };

                match item.handle() {
                    TimelineItemHandle::Local(handle) => {
                        // If there is a local item that hasn't been sent yet, abort the upload
                        handle.abort().await.map_err(RoomSendQueueError::StorageError)?;
                        return Ok(());
                    }
                    TimelineItemHandle::Remote(event_id) => event_id.to_owned(),
                }
            }
            TimelineEventItemId::EventId(event_id) => event_id.to_owned(),
        };
        self.room()
            .redact(&event_id, reason, None)
            .await
            .map_err(|e| Error::RedactError(RedactError::HttpError(e)))?;

        Ok(())
    }

    /// Redact an event.
    ///
    /// # Returns
    ///
    /// - Returns `Ok(true)` if the redact happened.
    /// - Returns `Ok(false)` if the redact targets an item that has no local
    ///   nor matching remote item.
    /// - Returns an error if there was an issue sending the redaction event, or
    ///   interacting with the sending queue.
    pub async fn redact(
        &self,
        event: &EventTimelineItem,
        reason: Option<&str>,
    ) -> Result<bool, Error> {
        let event_id = match event.identifier() {
            TimelineEventItemId::TransactionId(_) => {
                // See if we have an up-to-date timeline item with that transaction id.
                match event.handle() {
                    TimelineItemHandle::Remote(event_id) => event_id.to_owned(),
                    TimelineItemHandle::Local(handle) => {
                        return Ok(handle
                            .abort()
                            .await
                            .map_err(RoomSendQueueError::StorageError)?);
                    }
                }
            }

            TimelineEventItemId::EventId(event_id) => event_id,
        };

        self.room()
            .redact(&event_id, reason, None)
            .await
            .map_err(RedactError::HttpError)
            .map_err(Error::RedactError)?;

        Ok(true)
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
        self.controller.fetch_in_reply_to_details(event_id).await
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
        self.controller.set_sender_profiles_pending().await;
        match self.room().sync_members().await {
            Ok(_) => {
                self.controller.update_missing_sender_profiles().await;
            }
            Err(e) => {
                self.controller.set_sender_profiles_error(Arc::new(e)).await;
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
        self.controller.latest_user_read_receipt(user_id).await
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
        self.controller.latest_user_read_receipt_timeline_event_id(user_id).await
    }

    /// Subscribe to changes in the read receipts of our own user.
    pub async fn subscribe_own_user_read_receipts_changed(&self) -> impl Stream<Item = ()> {
        self.controller.subscribe_own_user_read_receipts_changed().await
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
        if !self.controller.should_send_receipt(&receipt_type, &thread, &event_id).await {
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
                .controller
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
                .controller
                .should_send_receipt(&ReceiptType::Read, &ReceiptThread::Unthreaded, read_receipt)
                .await
            {
                receipts.public_read_receipt = None;
            }
        }

        if let Some(private_read_receipt) = &receipts.private_read_receipt {
            if !self
                .controller
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
        if let Some(event_id) = self.controller.latest_event_id().await {
            self.send_single_receipt(receipt_type, ReceiptThread::Unthreaded, event_id).await
        } else {
            trace!("can't mark room as read because there's no latest event id");
            Ok(false)
        }
    }

    /// Adds a new pinned event by sending an updated `m.room.pinned_events`
    /// event containing the new event id.
    ///
    /// Returns `true` if we sent the request, `false` if the event was already
    /// pinned.
    pub async fn pin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = self.room().pinned_event_ids();
        let event_id = event_id.to_owned();
        if pinned_event_ids.contains(&event_id) {
            Ok(false)
        } else {
            pinned_event_ids.push(event_id);
            let content = RoomPinnedEventsEventContent::new(pinned_event_ids);
            self.room().send_state_event(content).await?;
            Ok(true)
        }
    }

    /// Adds a new pinned event by sending an updated `m.room.pinned_events`
    /// event without the event id we want to remove.
    ///
    /// Returns `true` if we sent the request, `false` if the event wasn't
    /// pinned.
    pub async fn unpin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = self.room().pinned_event_ids();
        let event_id = event_id.to_owned();
        if let Some(idx) = pinned_event_ids.iter().position(|e| *e == *event_id) {
            pinned_event_ids.remove(idx);
            let content = RoomPinnedEventsEventContent::new(pinned_event_ids);
            self.room().send_state_event(content).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Test helpers, likely not very useful in production.
#[doc(hidden)]
impl Timeline {
    /// Get the current list of timeline items.
    pub async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.controller.items().await
    }

    pub async fn subscribe_filter_map<U: Clone>(
        &self,
        f: impl Fn(Arc<TimelineItem>) -> Option<U>,
    ) -> (Vector<U>, impl Stream<Item = VectorDiff<U>>) {
        let (items, stream) = self.controller.subscribe_filter_map(f).await;
        let stream = TimelineStream::new(stream, self.drop_handle.clone());
        (items, stream)
    }
}

#[derive(Debug)]
struct TimelineDropHandle {
    client: Client,
    event_handler_handles: Vec<EventHandlerHandle>,
    room_update_join_handle: JoinHandle<()>,
    pinned_events_join_handle: Option<JoinHandle<()>>,
    room_key_from_backups_join_handle: JoinHandle<()>,
    room_key_backup_enabled_join_handle: JoinHandle<()>,
    local_echo_listener_handle: JoinHandle<()>,
    _event_cache_drop_handle: Arc<EventCacheDropHandles>,
    encryption_changes_handle: JoinHandle<()>,
}

impl Drop for TimelineDropHandle {
    fn drop(&mut self) {
        for handle in self.event_handler_handles.drain(..) {
            self.client.remove_event_handler(handle);
        }

        if let Some(handle) = self.pinned_events_join_handle.take() {
            handle.abort()
        };

        self.local_echo_listener_handle.abort();
        self.room_update_join_handle.abort();
        self.room_key_from_backups_join_handle.abort();
        self.room_key_backup_enabled_join_handle.abort();
        self.encryption_changes_handle.abort();
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
