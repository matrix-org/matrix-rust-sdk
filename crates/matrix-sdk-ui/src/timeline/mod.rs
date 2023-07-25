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

use std::{ops::Deref, pin::Pin, sync::Arc, task::Poll, time::Duration};

use async_std::sync::{Condvar, Mutex};
use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use imbl::Vector;
use matrix_sdk::{
    attachment::AttachmentConfig,
    event_handler::EventHandlerHandle,
    executor::JoinHandle,
    room::{MessagesOptions, Receipts, Room},
    Client, Result,
};
use matrix_sdk_base::RoomState;
use mime::Mime;
use pin_project_lite::pin_project;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    assign,
    events::{
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread},
        relation::Annotation,
        room::{message::sanitize::HtmlSanitizerMode, redaction::RoomRedactionEventContent},
        AnyMessageLikeEventContent,
    },
    EventId, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, instrument, warn};

mod builder;
mod event_handler;
mod event_item;
mod futures;
mod inner;
mod item;
mod pagination;
mod queue;
mod reactions;
mod read_receipts;
mod sliding_sync_ext;
#[cfg(test)]
mod tests;
#[cfg(feature = "e2e-encryption")]
mod to_device;
mod traits;
mod virtual_item;

pub use self::{
    builder::TimelineBuilder,
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, EventItemOrigin,
        EventSendState, EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange,
        Message, OtherState, Profile, ReactionGroup, RepliedToEvent, RoomMembershipChange, Sticker,
        TimelineDetails, TimelineItemContent,
    },
    futures::SendAttachment,
    item::{TimelineItem, TimelineItemKind},
    pagination::{PaginationOptions, PaginationOutcome},
    reactions::ReactionSenderData,
    sliding_sync_ext::SlidingSyncRoomExt,
    traits::RoomExt,
    virtual_item::VirtualTimelineItem,
};
use self::{
    event_item::EventTimelineItemKind,
    inner::{ReactionAction, TimelineInner, TimelineInnerState},
    queue::LocalMessage,
    reactions::ReactionToggleResult,
};

/// The default sanitizer mode used when sanitizing HTML.
const DEFAULT_SANITIZER_MODE: HtmlSanitizerMode = HtmlSanitizerMode::Compat;

/// A high-level view into a regular¹ room's contents.
///
/// ¹ This type is meant to be used in the context of rooms without a
/// `room_type`, that is rooms that are primarily used to exchange text
/// messages.
#[derive(Debug)]
pub struct Timeline {
    inner: TimelineInner<Room>,

    start_token: Arc<Mutex<Option<String>>>,
    start_token_condvar: Arc<Condvar>,
    /// Observable for whether a pagination is currently running
    back_pagination_status: SharedObservable<BackPaginationStatus>,

    _end_token: Mutex<Option<String>>,
    msg_sender: Sender<LocalMessage>,
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

impl Timeline {
    pub(crate) fn builder(room: &Room) -> TimelineBuilder {
        TimelineBuilder::new(room)
    }

    fn room(&self) -> &Room {
        self.inner.room()
    }

    /// Clear all timeline items, and reset pagination parameters.
    pub async fn clear(&self) {
        let mut start_lock = self.start_token.lock().await;
        let mut end_lock = self._end_token.lock().await;

        *start_lock = None;
        *end_lock = None;

        self.inner.clear().await;
    }

    /// Subscribe to the back-pagination status of the timeline.
    pub fn back_pagination_status(&self) -> Subscriber<BackPaginationStatus> {
        self.back_pagination_status.subscribe()
    }

    /// Add more events to the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id(), ?options))]
    pub async fn paginate_backwards(&self, mut options: PaginationOptions<'_>) -> Result<()> {
        let mut start_lock = self.start_token.lock().await;
        if start_lock.is_none()
            && self.back_pagination_status.get() == BackPaginationStatus::TimelineStartReached
        {
            warn!("Start of timeline reached, ignoring backwards-pagination request");
            return Ok(());
        }

        self.back_pagination_status.set(BackPaginationStatus::Paginating);

        if start_lock.is_none() && options.wait_for_token {
            info!("No prev_batch token, waiting");
            (start_lock, _) = self
                .start_token_condvar
                .wait_timeout_until(start_lock, Duration::from_secs(3), |tok| tok.is_some())
                .await;

            if start_lock.is_none() {
                debug!("Waiting for prev_batch token timed out after 3s");
            }
        }

        let mut from = start_lock.clone();
        let mut outcome = PaginationOutcome::new();

        while let Some(limit) = options.next_event_limit(outcome) {
            let messages = self
                .room()
                .messages(assign!(MessagesOptions::backward(), {
                    from,
                    limit: limit.into(),
                }))
                .await
                .map_err(|e| {
                    self.back_pagination_status.set(BackPaginationStatus::Idle);
                    e
                })?;

            let process_events_result = async {
                outcome.events_received = messages.chunk.len().try_into().ok()?;
                outcome.total_events_received =
                    outcome.total_events_received.checked_add(outcome.events_received)?;
                outcome.items_added = 0;
                outcome.items_updated = 0;

                for room_ev in messages.chunk {
                    let res = self.inner.handle_back_paginated_event(room_ev).await;
                    outcome.items_added = outcome.items_added.checked_add(res.item_added as u16)?;
                    outcome.items_updated = outcome.items_updated.checked_add(res.items_updated)?;
                }

                outcome.total_items_added =
                    outcome.total_items_added.checked_add(outcome.items_added)?;
                outcome.total_items_updated =
                    outcome.total_items_updated.checked_add(outcome.items_updated)?;

                Some(())
            }
            .await;

            from = messages.end;

            if from.is_none() {
                break;
            }

            if process_events_result.is_none() {
                error!("Received an excessive number of events, ending pagination (u16 overflow)");
                break;
            }
        }

        let status = if from.is_some() {
            BackPaginationStatus::Idle
        } else {
            BackPaginationStatus::TimelineStartReached
        };
        self.back_pagination_status.set(status);
        *start_lock = from;

        Ok(())
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

    /// Get the current list of timeline items. Do not use this in production!
    #[cfg(feature = "testing")]
    pub async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.inner.items().await
    }

    /// Get the latest of the timeline's event items.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner.items().await.last()?.as_event().cloned()
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

    #[cfg(feature = "testing")]
    pub async fn subscribe_filter_map<U: Clone>(
        &self,
        f: impl Fn(Arc<TimelineItem>) -> Option<U>,
    ) -> (Vector<U>, impl Stream<Item = VectorDiff<U>>) {
        let (items, stream) = self.inner.subscribe_filter_map(f).await;
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
    /// * `txn_id` - A locally-unique ID describing a message transaction with
    ///   the homeserver. Unless you're doing something special, you can pass in
    ///   `None` which will create a suitable one for you automatically.
    ///     * On the sending side, this field is used for re-trying earlier
    ///       failed transactions. Subsequent messages *must never* re-use an
    ///       earlier transaction ID.
    ///     * On the receiving side, the field is used for recognizing our own
    ///       messages when they arrive down the sync: the server includes the
    ///       ID in the [`MessageLikeUnsigned`] field `transaction_id` of the
    ///       corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///       device. Other devices will not see it.
    ///
    /// [`MessageLikeUnsigned`]: ruma::events::MessageLikeUnsigned
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    #[instrument(skip(self, content), fields(room_id = ?self.room().room_id()))]
    pub async fn send(&self, content: AnyMessageLikeEventContent, txn_id: Option<&TransactionId>) {
        let txn_id = txn_id.map_or_else(TransactionId::new, ToOwned::to_owned);
        self.inner.handle_local_event(txn_id.clone(), content.clone()).await;
        if self.msg_sender.send(LocalMessage { content, txn_id }).await.is_err() {
            error!("Internal error: timeline message receiver is closed");
        }
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

        // The local echo may have been updated while a reaction is is flight
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
            return ReactionToggleResult::RedactFailure { event_id: event_id.to_owned() };
        }

        let txn_id = TransactionId::new();
        let no_reason = RoomRedactionEventContent::default();

        let response = room.redact(event_id, no_reason.reason.as_deref(), Some(txn_id)).await;

        match response {
            Ok(_) => ReactionToggleResult::RedactSuccess,
            Err(_) => ReactionToggleResult::RedactFailure { event_id: event_id.to_owned() },
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
            return ReactionToggleResult::AddFailure { txn_id };
        }

        let event_content =
            AnyMessageLikeEventContent::Reaction(ReactionEventContent::from(annotation.clone()));
        let response = room.send(event_content, Some(&txn_id)).await;

        match response {
            Ok(response) => {
                ReactionToggleResult::AddSuccess { event_id: response.event_id, txn_id }
            }
            Err(_) => ReactionToggleResult::AddFailure { txn_id },
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
    /// * `url` - The url for the file to be sent
    ///
    /// * `mime_type` - The attachment's mime type
    ///
    /// * `config` - An attachment configuration object containing details about
    ///   the attachment
    /// like a thumbnail, its size, duration etc.
    pub fn send_attachment(
        &self,
        url: String,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> SendAttachment<'_> {
        SendAttachment::new(self, url, mime_type, config)
    }

    /// Retry sending a message that previously failed to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - The transaction ID of a local echo timeline item that has a
    ///   `send_state()` of `SendState::FailedToSend { .. }`
    pub async fn retry_send(&self, txn_id: &TransactionId) -> Result<(), Error> {
        macro_rules! error_return {
            ($msg:literal) => {{
                error!($msg);
                return Ok(());
            }};
        }

        let item = self.inner.prepare_retry(txn_id).await.ok_or(Error::RetryEventNotInTimeline)?;
        let content = match item {
            TimelineItemContent::Message(msg) => {
                AnyMessageLikeEventContent::RoomMessage(msg.into())
            }
            TimelineItemContent::RedactedMessage => {
                error_return!("Invalid state: attempting to retry a redacted message");
            }
            TimelineItemContent::Sticker(sticker) => {
                AnyMessageLikeEventContent::Sticker(sticker.content)
            }
            TimelineItemContent::UnableToDecrypt(_) => {
                error_return!("Invalid state: attempting to retry a UTD item");
            }
            TimelineItemContent::MembershipChange(_)
            | TimelineItemContent::ProfileChange(_)
            | TimelineItemContent::OtherState(_) => {
                error_return!("Retrying state events is not currently supported");
            }
            TimelineItemContent::FailedToParseMessageLike { .. }
            | TimelineItemContent::FailedToParseState { .. } => {
                error_return!("Invalid state: attempting to retry a failed-to-parse item");
            }
        };

        let txn_id = txn_id.to_owned();
        if self.msg_sender.send(LocalMessage { content, txn_id }).await.is_err() {
            error!("Internal error: timeline message receiver is closed");
        }

        Ok(())
    }

    /// Discard a local echo for a message that failed to send.
    ///
    /// Returns whether the local echo with the given transaction ID was found.
    ///
    /// # Argument
    ///
    /// * `txn_id` - The transaction ID of a local echo timeline item that has a
    ///   `send_state()` of `SendState::FailedToSend { .. }`. *Note:* A send
    ///   state of `SendState::NotYetSent` might be supported in the future as
    ///   well, but there can be no guarantee for that actually stopping the
    ///   event from reaching the server.
    pub async fn cancel_send(&self, txn_id: &TransactionId) -> bool {
        self.inner.discard_local_echo(txn_id).await
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
                self.inner.update_sender_profiles().await;
            }
            Err(e) => {
                self.inner.set_sender_profiles_error(Arc::new(e)).await;
            }
        }
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Contrary to [`Room::user_receipt()`] that only keeps track of read
    /// receipts received from the homeserver, this keeps also track of implicit
    /// read receipts in this timeline, i.e. when a room member sends an event.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.inner.latest_user_read_receipt(user_id).await
    }

    /// Send the given receipt.
    ///
    /// This uses [`Room::send_single_receipt`] internally, but checks
    /// first if the receipt points to an event in this timeline that is more
    /// recent than the current ones, to avoid unnecessary requests.
    #[instrument(skip(self))]
    pub async fn send_single_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: OwnedEventId,
    ) -> Result<()> {
        if !self.inner.should_send_receipt(&receipt_type, &thread, &event_id).await {
            return Ok(());
        }

        self.room().send_single_receipt(receipt_type, thread, event_id).await
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
}

#[derive(Debug)]
struct TimelineDropHandle {
    client: Client,
    event_handler_handles: Vec<EventHandlerHandle>,
    room_update_join_handle: JoinHandle<()>,
}

impl Drop for TimelineDropHandle {
    fn drop(&mut self) {
        for handle in self.event_handler_handles.drain(..) {
            self.client.remove_event_handler(handle);
        }
        self.room_update_join_handle.abort();
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

struct EventTimelineItemWithId<'a> {
    inner: &'a EventTimelineItem,
    internal_id: u64,
}

impl<'a> EventTimelineItemWithId<'a> {
    fn with_inner_kind(&self, kind: impl Into<EventTimelineItemKind>) -> Arc<TimelineItem> {
        Arc::new(TimelineItem {
            kind: self.inner.with_kind(kind).into(),
            internal_id: self.internal_id,
        })
    }
}

impl Deref for EventTimelineItemWithId<'_> {
    type Target = EventTimelineItem;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
fn rfind_event_item(
    items: &Vector<Arc<TimelineItem>>,
    mut f: impl FnMut(&EventTimelineItem) -> bool,
) -> Option<(usize, EventTimelineItemWithId<'_>)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            Some((
                idx,
                EventTimelineItemWithId { inner: item.as_event()?, internal_id: item.internal_id },
            ))
        })
        .rfind(|(_, it)| f(it.inner))
}

fn rfind_event_by_id<'a>(
    items: &'a Vector<Arc<TimelineItem>>,
    event_id: &EventId,
) -> Option<(usize, EventTimelineItemWithId<'a>)> {
    rfind_event_item(items, |it| it.event_id() == Some(event_id))
}

fn find_read_marker(items: &Vector<Arc<TimelineItem>>) -> Option<usize> {
    items.iter().rposition(|item| item.is_read_marker())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackPaginationStatus {
    Idle,
    Paginating,
    TimelineStartReached,
}

/// Errors specific to the timeline.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The requested event with a remote echo is not in the timeline.
    #[error("Event with remote echo not found in timeline")]
    RemoteEventNotInTimeline,

    /// Can't find an event with the given transaction ID, can't retry.
    #[error("Event not found, can't retry sending")]
    RetryEventNotInTimeline,

    /// The event is currently unsupported for this use case.
    #[error("Unsupported event")]
    UnsupportedEvent,

    /// Couldn't read the attachment data from the given URL
    #[error("Invalid attachment data")]
    InvalidAttachmentData,

    /// The attachment file name used as a body is invalid
    #[error("Invalid attachment file name")]
    InvalidAttachmentFileName,

    /// The attachment could not be sent
    #[error("Failed sending attachment")]
    FailedSendingAttachment,

    /// The reaction could not be toggled
    #[error("Failed toggling reaction")]
    FailedToToggleReaction,

    /// The room is not in a joined state.
    #[error("Room is not joined")]
    RoomNotJoined,

    /// Could not get user
    #[error("User ID is not available")]
    UserIdNotAvailable,
}

/// Result of comparing events position in the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelativePosition {
    /// Event B is after (more recent than) event A.
    After,
    /// They are the same event.
    Same,
    /// Event B is before (older than) event A.
    Before,
}

fn compare_events_positions(
    event_a: &EventId,
    event_b: &EventId,
    timeline_items: &Vector<Arc<TimelineItem>>,
) -> Option<RelativePosition> {
    if event_a == event_b {
        return Some(RelativePosition::Same);
    }

    let (pos_event_a, _) = rfind_event_by_id(timeline_items, event_a)?;
    let (pos_event_b, _) = rfind_event_by_id(timeline_items, event_b)?;

    if pos_event_a > pos_event_b {
        Some(RelativePosition::Before)
    } else {
        Some(RelativePosition::After)
    }
}
