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

use std::{fs, path::PathBuf, sync::Arc};

use algorithms::rfind_event_by_item_id;
use event_item::TimelineItemHandle;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use imbl::Vector;
use matrix_sdk::{
    attachment::AttachmentConfig,
    event_cache::{EventCacheDropHandles, RoomEventCache},
    event_handler::EventHandlerHandle,
    executor::JoinHandle,
    room::{edit::EditedContent, reply::EnforceThread, Receipts, Room},
    send_queue::{RoomSendQueueError, SendHandle},
    Client, Result,
};
use mime::Mime;
use pinned_events_loader::PinnedEventsRoom;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    events::{
        poll::unstable_start::{NewUnstablePollStartEventContent, UnstablePollStartEventContent},
        receipt::{Receipt, ReceiptThread},
        room::{
            message::RoomMessageEventContentWithoutRelation,
            pinned_events::RoomPinnedEventsEventContent,
        },
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
    },
    EventId, OwnedEventId, RoomVersionId, UserId,
};
use subscriber::TimelineWithDropHandle;
use thiserror::Error;
use tracing::{instrument, trace, warn};

use self::{
    algorithms::rfind_event_by_id, controller::TimelineController, futures::SendAttachment,
};

mod algorithms;
mod builder;
mod controller;
mod date_dividers;
mod error;
mod event_handler;
mod event_item;
pub mod event_type_filter;
pub mod futures;
mod item;
mod pagination;
mod pinned_events_loader;
mod subscriber;
#[cfg(test)]
mod tests;
mod to_device;
mod traits;
mod virtual_item;

pub use self::{
    builder::TimelineBuilder,
    controller::default_event_filter,
    error::*,
    event_item::{
        AnyOtherFullStateEventContent, EncryptedMessage, EventItemOrigin, EventSendState,
        EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange, Message,
        MsgLikeContent, MsgLikeKind, OtherState, PollResult, PollState, Profile, ReactionInfo,
        ReactionStatus, ReactionsByKeyBySender, RepliedToEvent, RoomMembershipChange,
        RoomPinnedEventsChange, Sticker, TimelineDetails, TimelineEventItemId, TimelineItemContent,
    },
    event_type_filter::TimelineEventTypeFilter,
    item::{TimelineItem, TimelineItemKind, TimelineUniqueId},
    traits::RoomExt,
    virtual_item::VirtualTimelineItem,
};

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

impl TimelineFocus {
    pub(super) fn debug_string(&self) -> String {
        match self {
            TimelineFocus::Live => "live".to_owned(),
            TimelineFocus::Event { target, .. } => format!("permalink:{target}"),
            TimelineFocus::PinnedEvents { .. } => "pinned-events".to_owned(),
        }
    }
}

/// Changes how dividers get inserted, either in between each day or in between
/// each month
#[derive(Debug, Clone)]
pub enum DateDividerMode {
    Daily,
    Monthly,
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
            .retry_event_decryption(Some(session_ids.into_iter().map(Into::into).collect()))
            .await;
    }

    #[tracing::instrument(skip(self))]
    async fn retry_decryption_for_all_events(&self) {
        self.controller.retry_event_decryption(None).await;
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

    /// Get the latest of the timeline's event items.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        if self.controller.is_live().await {
            self.controller.items().await.last()?.as_event().cloned()
        } else {
            None
        }
    }

    /// Get the current timeline items, along with a stream of updates of
    /// timeline items.
    ///
    /// The stream produces `Vec<VectorDiff<_>>`, which means multiple updates
    /// at once. There are no delays, it consumes as many updates as possible
    /// and batches them.
    pub async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>) {
        let (items, stream) = self.controller.subscribe().await;
        let stream = TimelineWithDropHandle::new(stream, self.drop_handle.clone());
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
    /// * `event_id` - The ID of the event to reply to
    ///
    /// * `enforce_thread` - Whether to enforce a thread relation on the reply
    #[instrument(skip(self, content))]
    pub async fn send_reply(
        &self,
        content: RoomMessageEventContentWithoutRelation,
        event_id: OwnedEventId,
        enforce_thread: EnforceThread,
    ) -> Result<(), Error> {
        let content = self.room().make_reply_event(content, &event_id, enforce_thread).await?;
        self.send(content.into()).await?;
        Ok(())
    }

    /// Edit an event given its [`TimelineEventItemId`] and some new content.
    ///
    /// Only supports events for which [`EventTimelineItem::is_editable()`]
    /// returns `true`.
    #[instrument(skip(self, new_content))]
    pub async fn edit(
        &self,
        item_id: &TimelineEventItemId,
        new_content: EditedContent,
    ) -> Result<(), Error> {
        let items = self.items().await;
        let Some((_pos, item)) = rfind_event_by_item_id(&items, item_id) else {
            return Err(Error::EventNotInTimeline(item_id.clone()));
        };

        match item.handle() {
            TimelineItemHandle::Remote(event_id) => {
                let content = self
                    .room()
                    .make_edit_event(event_id, new_content)
                    .await
                    .map_err(EditError::RoomError)?;
                self.send(content).await?;
                Ok(())
            }

            TimelineItemHandle::Local(handle) => {
                // Relations are filled by the editing code itself.
                let new_content: AnyMessageLikeEventContent = match new_content {
                    EditedContent::RoomMessage(message) => {
                        if item.content.is_message() {
                            AnyMessageLikeEventContent::RoomMessage(message.into())
                        } else {
                            return Err(EditError::ContentMismatch {
                                original: item.content.debug_string().to_owned(),
                                new: "a message".to_owned(),
                            }
                            .into());
                        }
                    }

                    EditedContent::PollStart { new_content, .. } => {
                        if item.content.is_poll() {
                            AnyMessageLikeEventContent::UnstablePollStart(
                                UnstablePollStartEventContent::New(
                                    NewUnstablePollStartEventContent::new(new_content),
                                ),
                            )
                        } else {
                            return Err(EditError::ContentMismatch {
                                original: item.content.debug_string().to_owned(),
                                new: "a poll".to_owned(),
                            }
                            .into());
                        }
                    }

                    EditedContent::MediaCaption { caption, formatted_caption, mentions } => {
                        if handle
                            .edit_media_caption(caption, formatted_caption, mentions)
                            .await
                            .map_err(RoomSendQueueError::StorageError)?
                        {
                            return Ok(());
                        }
                        return Err(EditError::InvalidLocalEchoState.into());
                    }
                };

                if !handle.edit(new_content).await.map_err(RoomSendQueueError::StorageError)? {
                    return Err(EditError::InvalidLocalEchoState.into());
                }

                Ok(())
            }
        }
    }

    /// Toggle a reaction on an event.
    ///
    /// Adds or redacts a reaction based on the state of the reaction at the
    /// time it is called.
    ///
    /// When redacting a previous reaction, the redaction reason is not set.
    ///
    /// Ensures that only one reaction is sent at a time to avoid race
    /// conditions and spamming the homeserver with requests.
    pub async fn toggle_reaction(
        &self,
        item_id: &TimelineEventItemId,
        reaction_key: &str,
    ) -> Result<(), Error> {
        self.controller.toggle_reaction_local(item_id, reaction_key).await?;
        Ok(())
    }

    /// Sends an attachment to the room.
    ///
    /// It does not currently support local echoes.
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// The attachment and its optional thumbnail are stored in the media cache
    /// and can be retrieved at any time, by calling
    /// [`Media::get_media_content()`] with the `MediaSource` that can be found
    /// in the corresponding `TimelineEventItem`, and using a
    /// `MediaFormat::File`.
    ///
    /// # Arguments
    ///
    /// * `source` - The source of the attachment to send.
    ///
    /// * `mime_type` - The attachment's mime type.
    ///
    /// * `config` - An attachment configuration object containing details about
    ///   the attachment like a thumbnail, its size, duration etc.
    ///
    /// [`Media::get_media_content()`]: matrix_sdk::Media::get_media_content
    #[instrument(skip_all)]
    pub fn send_attachment(
        &self,
        source: impl Into<AttachmentSource>,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> SendAttachment<'_> {
        SendAttachment::new(self, source.into(), mime_type, config)
    }

    /// Redact an event given its [`TimelineEventItemId`] and an optional
    /// reason.
    pub async fn redact(
        &self,
        item_id: &TimelineEventItemId,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let items = self.items().await;
        let Some((_pos, event)) = rfind_event_by_item_id(&items, item_id) else {
            return Err(RedactError::ItemNotFound(item_id.clone()).into());
        };

        match event.handle() {
            TimelineItemHandle::Remote(event_id) => {
                self.room().redact(event_id, reason, None).await.map_err(RedactError::HttpError)?;
            }
            TimelineItemHandle::Local(handle) => {
                if !handle.abort().await.map_err(RoomSendQueueError::StorageError)? {
                    return Err(RedactError::InvalidLocalEchoState.into());
                }
            }
        }

        Ok(())
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
    /// This method will first try to get the pinned events from the current
    /// room's state and if it fails to do so it'll try to load them from the
    /// homeserver.
    ///
    /// Returns `true` if we pinned the event, `false` if the event was already
    /// pinned.
    pub async fn pin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = if let Some(event_ids) = self.room().pinned_event_ids() {
            event_ids
        } else {
            self.room().load_pinned_events().await?.unwrap_or_default()
        };
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

    /// Removes a pinned event by sending an updated `m.room.pinned_events`
    /// event without the event id we want to remove.
    ///
    /// This method will first try to get the pinned events from the current
    /// room's state and if it fails to do so it'll try to load them from the
    /// homeserver.
    ///
    /// Returns `true` if we unpinned the event, `false` if the event wasn't
    /// pinned before.
    pub async fn unpin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = if let Some(event_ids) = self.room().pinned_event_ids() {
            event_ids
        } else {
            self.room().load_pinned_events().await?.unwrap_or_default()
        };
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
        let stream = TimelineWithDropHandle::new(stream, self.drop_handle.clone());
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
    room_keys_received_join_handle: JoinHandle<()>,
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
        self.room_keys_received_join_handle.abort();
        self.encryption_changes_handle.abort();
    }
}

pub type TimelineEventFilterFn =
    dyn Fn(&AnySyncTimelineEvent, &RoomVersionId) -> bool + Send + Sync;

/// A source for sending an attachment.
///
/// The [`AttachmentSource::File`] variant can be constructed from any type that
/// implements `Into<PathBuf>`.
#[derive(Debug, Clone)]
pub enum AttachmentSource {
    /// The data of the attachment.
    Data {
        /// The bytes of the attachment.
        bytes: Vec<u8>,

        /// The filename of the attachment.
        filename: String,
    },

    /// An attachment loaded from a file.
    ///
    /// The bytes and the filename will be read from the file at the given path.
    File(PathBuf),
}

impl AttachmentSource {
    /// Try to convert this attachment source into a `(bytes, filename)` tuple.
    pub(crate) fn try_into_bytes_and_filename(self) -> Result<(Vec<u8>, String), Error> {
        match self {
            Self::Data { bytes, filename } => Ok((bytes, filename)),
            Self::File(path) => {
                let filename = path
                    .file_name()
                    .ok_or(Error::InvalidAttachmentFileName)?
                    .to_str()
                    .ok_or(Error::InvalidAttachmentFileName)?
                    .to_owned();
                let bytes = fs::read(&path).map_err(|_| Error::InvalidAttachmentData)?;
                Ok((bytes, filename))
            }
        }
    }
}

impl<P> From<P> for AttachmentSource
where
    P: Into<PathBuf>,
{
    fn from(value: P) -> Self {
        Self::File(value.into())
    }
}
