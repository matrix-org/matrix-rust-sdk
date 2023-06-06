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

use std::{fs, path::Path, pin::Pin, sync::Arc, task::Poll};

use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::TryFutureExt;
use imbl::Vector;
use matrix_sdk::{
    attachment::AttachmentConfig,
    event_handler::EventHandlerHandle,
    executor::JoinHandle,
    room::{self, MessagesOptions, Receipts, Room},
    Client, Result,
};
use mime::Mime;
use pin_project_lite::pin_project;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    assign,
    events::{
        receipt::{Receipt, ReceiptThread},
        room::message::sanitize::HtmlSanitizerMode,
        AnyMessageLikeEventContent,
    },
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, TransactionId, UserId,
};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{error, instrument, warn};

mod builder;
mod event_handler;
mod event_item;
mod inner;
mod pagination;
mod read_receipts;
mod room_ext;
#[cfg(feature = "experimental-sliding-sync")]
mod sliding_sync_ext;
#[cfg(test)]
mod tests;
#[cfg(feature = "e2e-encryption")]
mod to_device;
mod traits;
mod virtual_item;

pub(crate) use self::builder::TimelineBuilder;
use self::inner::{TimelineInner, TimelineInnerState};
#[cfg(feature = "experimental-sliding-sync")]
pub use self::sliding_sync_ext::SlidingSyncRoomExt;
pub use self::{
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, EventSendState,
        EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange, Message,
        OtherState, Profile, ReactionGroup, RepliedToEvent, RoomMembershipChange, Sticker,
        TimelineDetails, TimelineItemContent,
    },
    pagination::{PaginationOptions, PaginationOutcome},
    room_ext::RoomExt,
    virtual_item::VirtualTimelineItem,
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
    inner: Arc<TimelineInner<room::Common>>,
    start_token: Mutex<Option<String>>,
    _end_token: Mutex<Option<String>>,
    drop_handle: Arc<TimelineDropHandle>,
}

impl Timeline {
    pub(crate) fn builder(room: &room::Common) -> TimelineBuilder {
        TimelineBuilder::new(room)
    }

    fn room(&self) -> &room::Common {
        self.inner.room()
    }

    /// Clear all timeline items, and reset pagination parameters.
    #[cfg(feature = "experimental-sliding-sync")]
    pub async fn clear(&self) {
        let mut start_lock = self.start_token.lock().await;
        let mut end_lock = self._end_token.lock().await;

        *start_lock = None;
        *end_lock = None;

        self.inner.clear().await;
    }

    /// Add more events to the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id(), ?options))]
    pub async fn paginate_backwards(&self, mut options: PaginationOptions<'_>) -> Result<()> {
        let mut start_lock = self.start_token.lock().await;
        if start_lock.is_none()
            && self.inner.items().await.front().map_or(false, |item| item.is_timeline_start())
        {
            warn!("Start of timeline reached, ignoring backwards-pagination request");
            return Ok(());
        }

        self.inner.add_loading_indicator().await;

        let mut from = start_lock.clone();
        let mut outcome = PaginationOutcome::new();

        while let Some(limit) = options.next_event_limit(outcome) {
            let messages = self
                .room()
                .messages(assign!(MessagesOptions::backward(), {
                    from,
                    limit: limit.into(),
                }))
                .await?;

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

        self.inner.remove_loading_indicator(from.is_some()).await;
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
    pub async fn retry_decryption<'a, S: AsRef<str> + 'a>(
        &'a self,
        session_ids: impl IntoIterator<Item = &'a S>,
    ) {
        self.inner
            .retry_event_decryption(
                self.room(),
                Some(session_ids.into_iter().map(AsRef::as_ref).collect()),
            )
            .await;
    }

    #[cfg(feature = "e2e-encryption")]
    #[tracing::instrument(skip(self))]
    async fn retry_decryption_for_all_events(&self) {
        self.inner.retry_event_decryption(self.room(), None).await;
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

        let send_state = match Room::from(self.room().clone()) {
            Room::Joined(room) => {
                let response = room.send(content, Some(&txn_id)).await;

                match response {
                    Ok(response) => EventSendState::Sent { event_id: response.event_id },
                    Err(error) => EventSendState::SendingFailed { error: Arc::new(error) },
                }
            }
            _ => {
                EventSendState::SendingFailed {
                    // FIXME: Probably not exactly right
                    error: Arc::new(matrix_sdk::Error::InconsistentState),
                }
            }
        };

        self.inner.update_event_send_state(&txn_id, send_state).await;
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
    pub async fn send_attachment(
        &self,
        url: String,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Result<(), Error> {
        let Room::Joined(room) = Room::from(self.room().clone()) else {
            return Err(Error::RoomNotJoined);
        };

        let body =
            Path::new(&url).file_name().ok_or(Error::InvalidAttachmentFileName)?.to_str().unwrap();
        let data = fs::read(&url).map_err(|_| Error::InvalidAttachmentData)?;

        room.send_attachment(body, &mime_type, data, config)
            .map_err(|_| Error::FailedSendingAttachment)
            .await?;

        Ok(())
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

        let send_state = match Room::from(self.room().clone()) {
            Room::Joined(room) => {
                let response = room.send(content, Some(txn_id)).await;

                match response {
                    Ok(response) => EventSendState::Sent { event_id: response.event_id },
                    Err(error) => EventSendState::SendingFailed { error: Arc::new(error) },
                }
            }
            _ => {
                EventSendState::SendingFailed {
                    // FIXME: Probably not exactly right
                    error: Arc::new(matrix_sdk::Error::InconsistentState),
                }
            }
        };

        self.inner.update_event_send_state(txn_id, send_state).await;

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
    /// Contrary to [`Common::user_receipt()`](room::Common::user_receipt) that
    /// only keeps track of read receipts received from the homeserver, this
    /// keeps also track of implicit read receipts in this timeline, i.e.
    /// when a room member sends an event.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.inner.latest_user_read_receipt(user_id).await
    }

    /// Send the given receipt.
    ///
    /// This uses [`Joined::send_single_receipt`] internally, but checks
    /// first if the receipt points to an event in this timeline that is more
    /// recent than the current ones, to avoid unnecessary requests.
    ///
    /// [`Joined::send_single_receipt`]: room::Joined::send_single_receipt
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

        let Room::Joined(room) = Room::from(self.room().clone()) else {
            // FIXME: Probably not exactly right
            return Err(matrix_sdk::Error::InconsistentState);
        };

        room.send_single_receipt(receipt_type, thread, event_id).await
    }

    /// Send the given receipts.
    ///
    /// This uses [`Joined::send_multiple_receipts`] internally, but checks
    /// first if the receipts point to events in this timeline that are more
    /// recent than the current ones, to avoid unnecessary requests.
    ///
    /// [`Joined::send_multiple_receipts`]: room::Joined::send_multiple_receipts
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

        let Room::Joined(room) = Room::from(self.room().clone()) else {
            // FIXME: Probably not exactly right
            return Err(matrix_sdk::Error::InconsistentState);
        };

        room.send_multiple_receipts(receipts).await
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
        event_handler_handles: Arc<TimelineDropHandle>,
    }
}

impl<S> TimelineStream<S> {
    fn new(inner: S, event_handler_handles: Arc<TimelineDropHandle>) -> Self {
        Self { inner, event_handler_handles }
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

/// A single entry in timeline.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TimelineItem {
    /// An event or aggregation of multiple events.
    Event(EventTimelineItem),
    /// An item that doesn't correspond to an event, for example the user's own
    /// read marker.
    Virtual(VirtualTimelineItem),
}

impl TimelineItem {
    /// Get the inner `EventTimelineItem`, if this is a `TimelineItem::Event`.
    pub fn as_event(&self) -> Option<&EventTimelineItem> {
        match self {
            Self::Event(v) => Some(v),
            _ => None,
        }
    }

    /// Get the inner `VirtualTimelineItem`, if this is a
    /// `TimelineItem::Virtual`.
    pub fn as_virtual(&self) -> Option<&VirtualTimelineItem> {
        match self {
            Self::Virtual(v) => Some(v),
            _ => None,
        }
    }

    /// Creates a new day divider from the given timestamp.
    fn day_divider(ts: MilliSecondsSinceUnixEpoch) -> Self {
        Self::Virtual(VirtualTimelineItem::DayDivider(ts))
    }

    fn read_marker() -> Self {
        Self::Virtual(VirtualTimelineItem::ReadMarker)
    }

    fn loading_indicator() -> Self {
        Self::Virtual(VirtualTimelineItem::LoadingIndicator)
    }

    fn timeline_start() -> Self {
        Self::Virtual(VirtualTimelineItem::TimelineStart)
    }

    fn is_virtual(&self) -> bool {
        matches!(self, Self::Virtual(_))
    }

    fn is_day_divider(&self) -> bool {
        matches!(self, Self::Virtual(VirtualTimelineItem::DayDivider(_)))
    }

    fn is_read_marker(&self) -> bool {
        matches!(self, Self::Virtual(VirtualTimelineItem::ReadMarker))
    }

    fn is_loading_indicator(&self) -> bool {
        matches!(self, Self::Virtual(VirtualTimelineItem::LoadingIndicator))
    }

    fn is_timeline_start(&self) -> bool {
        matches!(self, Self::Virtual(VirtualTimelineItem::TimelineStart))
    }
}

impl From<EventTimelineItem> for TimelineItem {
    fn from(item: EventTimelineItem) -> Self {
        Self::Event(item)
    }
}

impl From<VirtualTimelineItem> for TimelineItem {
    fn from(item: VirtualTimelineItem) -> Self {
        Self::Virtual(item)
    }
}

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
fn rfind_event_item(
    items: &Vector<Arc<TimelineItem>>,
    mut f: impl FnMut(&EventTimelineItem) -> bool,
) -> Option<(usize, &EventTimelineItem)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
        .rfind(|(_, it)| f(it))
}

fn rfind_event_by_id<'a>(
    items: &'a Vector<Arc<TimelineItem>>,
    event_id: &EventId,
) -> Option<(usize, &'a EventTimelineItem)> {
    rfind_event_item(items, |it| it.event_id() == Some(event_id))
}

fn find_read_marker(items: &Vector<Arc<TimelineItem>>) -> Option<usize> {
    items.iter().rposition(|item| item.is_read_marker())
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

    /// The room is not in a joined state.
    #[error("Room is not joined")]
    RoomNotJoined,
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
