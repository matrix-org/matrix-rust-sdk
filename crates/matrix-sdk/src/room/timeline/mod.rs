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

use std::sync::Arc;

use futures_core::Stream;
use futures_signals::signal_vec::{SignalVec, SignalVecExt, VecDiff};
#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_base::{deserialized_responses::EncryptionInfo, locks::Mutex};
use ruma::{
    assign,
    events::{fully_read::FullyReadEventContent, AnyMessageLikeEventContent},
    EventId, MilliSecondsSinceUnixEpoch, TransactionId,
};
use thiserror::Error;
use tracing::{error, instrument, warn};

use super::Joined;
use crate::{
    event_handler::EventHandlerHandle,
    room::{self, MessagesOptions},
    Result,
};

mod event_handler;
mod event_item;
mod inner;
mod pagination;
#[cfg(test)]
mod tests;
#[cfg(feature = "e2e-encryption")]
mod to_device;
mod virtual_item;

pub use self::{
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, EventSendState,
        EventTimelineItem, InReplyToDetails, MemberProfileChange, MembershipChange, Message,
        OtherState, Profile, ReactionGroup, RepliedToEvent, RoomMembershipChange, Sticker,
        TimelineDetails, TimelineItemContent,
    },
    pagination::{PaginationOptions, PaginationOutcome},
    virtual_item::VirtualTimelineItem,
};
use self::{
    inner::{TimelineInner, TimelineInnerMetadata},
    to_device::{handle_forwarded_room_key_event, handle_room_key_event},
};

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
    event_handler_handles: Vec<EventHandlerHandle>,
}

impl Drop for Timeline {
    fn drop(&mut self) {
        for handle in self.event_handler_handles.drain(..) {
            self.inner.room().client().remove_event_handler(handle);
        }
    }
}

impl Timeline {
    pub(super) fn new(room: &room::Common) -> Self {
        Self::from_inner(Arc::new(TimelineInner::new(room.to_owned())), None)
    }

    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) async fn with_events(
        room: &room::Common,
        prev_token: Option<String>,
        events: Vec<SyncTimelineEvent>,
    ) -> Self {
        let mut inner = TimelineInner::new(room.to_owned());
        inner.add_initial_events(events).await;

        let timeline = Self::from_inner(Arc::new(inner), prev_token);

        // The events we're injecting might be encrypted events, but we might
        // have received the room key to decrypt them while nobody was listening to the
        // `m.room_key` event, let's retry now.
        //
        // TODO: We could spawn a task here and put this into the background, though it
        // might not be worth it depending on the number of events we injected.
        // Some measuring needs to be done.
        #[cfg(feature = "e2e-encryption")]
        timeline.retry_decryption_for_all_events().await;

        timeline
    }

    fn from_inner(inner: Arc<TimelineInner>, prev_token: Option<String>) -> Timeline {
        let room = inner.room();

        let timeline_event_handle = room.add_event_handler({
            let inner = inner.clone();
            move |event, encryption_info: Option<EncryptionInfo>| {
                let inner = inner.clone();
                async move {
                    inner.handle_live_event(event, encryption_info).await;
                }
            }
        });

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.
        #[cfg(feature = "e2e-encryption")]
        let room_key_handle = room
            .client
            .add_event_handler(handle_room_key_event(inner.clone(), room.room_id().to_owned()));
        #[cfg(feature = "e2e-encryption")]
        let forwarded_room_key_handle = room.client.add_event_handler(
            handle_forwarded_room_key_event(inner.clone(), room.room_id().to_owned()),
        );

        let event_handler_handles = vec![
            timeline_event_handle,
            #[cfg(feature = "e2e-encryption")]
            room_key_handle,
            #[cfg(feature = "e2e-encryption")]
            forwarded_room_key_handle,
        ];

        Timeline {
            inner,
            start_token: Mutex::new(prev_token),
            _end_token: Mutex::new(None),
            event_handler_handles,
        }
    }

    fn room(&self) -> &room::Common {
        self.inner.room()
    }

    /// Enable tracking of the fully-read marker on this `Timeline`.
    pub async fn with_fully_read_tracking(mut self) -> Self {
        match self.room().account_data_static::<FullyReadEventContent>().await {
            Ok(Some(fully_read)) => match fully_read.deserialize() {
                Ok(fully_read) => {
                    self.inner.set_fully_read_event(fully_read.content.event_id).await;
                }
                Err(e) => {
                    error!("Failed to deserialize fully-read account data: {e}");
                }
            },
            Err(e) => {
                error!("Failed to get fully-read account data from the store: {e}");
            }
            _ => {}
        }

        let inner = self.inner.clone();
        let fully_read_handle = self.room().add_event_handler(move |event| {
            let inner = inner.clone();
            async move {
                inner.handle_fully_read(event).await;
            }
        });
        self.event_handler_handles.push(fully_read_handle);

        self
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
    ///
    /// # Arguments
    ///
    /// * `initial_pagination_size`: The number of events to fetch from the
    ///   server in the first pagination request. The server may choose return
    ///   fewer events, for example because the supplied number is too big or
    ///   the beginning of the visible timeline was reached.
    /// * `
    #[instrument(skip_all, fields(initial_pagination_size, room_id = ?self.room().room_id()))]
    pub async fn paginate_backwards(&self, mut opts: PaginationOptions<'_>) -> Result<()> {
        let mut start_lock = self.start_token.lock().await;
        if start_lock.is_none()
            && self.inner.items().first().map_or(false, |item| item.is_timeline_start())
        {
            warn!("Start of timeline reached, ignoring backwards-pagination request");
            return Ok(());
        }

        self.inner.add_loading_indicator();

        let mut from = start_lock.clone();
        let mut outcome = PaginationOutcome::new();

        while let Some(limit) = opts.next_event_limit(outcome) {
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

        self.inner.remove_loading_indicator(from.is_some());
        *start_lock = from;

        Ok(())
    }

    /// Retry decryption of previously un-decryptable events given a list of
    /// session IDs whose keys have been imported.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     room::timeline::Timeline, ruma::room_id,
    /// # };
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
                self.room().room_id(),
                self.room().client.olm_machine().expect("Olm machine wasn't started"),
                Some(session_ids.into_iter().map(AsRef::as_ref).collect()),
            )
            .await;
    }

    #[cfg(all(feature = "experimental-sliding-sync", feature = "e2e-encryption"))]
    async fn retry_decryption_for_all_events(&self) {
        self.inner
            .retry_event_decryption(
                self.room().room_id(),
                self.room().client.olm_machine().expect("Olm machine wasn't started"),
                None,
            )
            .await;
    }

    /// Get the latest of the timeline's event items.
    pub fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner.items().last()?.as_event().cloned()
    }

    /// Get a signal of the timeline's items.
    ///
    /// You can poll this signal to receive updates, the first of which will
    /// be the full list of items currently available.
    ///
    /// See [`SignalVecExt`](futures_signals::signal_vec::SignalVecExt) for a
    /// high-level API on top of [`SignalVec`].
    pub fn signal(&self) -> impl SignalVec<Item = Arc<TimelineItem>> {
        self.inner.items_signal()
    }

    /// Get a stream of timeline changes.
    ///
    /// This is a convenience shorthand for `timeline.signal().to_stream()`.
    pub fn stream(&self) -> impl Stream<Item = VecDiff<Arc<TimelineItem>>> {
        self.signal().to_stream()
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

        // If this room isn't actually in joined state, we'll get a server error.
        // Not ideal, but works for now.
        let room = Joined { inner: self.room().clone() };

        let response = room.send(content, Some(&txn_id)).await;

        let send_state = match response {
            Ok(response) => EventSendState::Sent { event_id: response.event_id },
            Err(error) => EventSendState::SendingFailed { error: Arc::new(error) },
        };
        self.inner.update_event_send_state(&txn_id, send_state);
    }

    /// Fetch unavailable details about the event with the given ID.
    ///
    /// This method only works for IDs of [`RemoteEventTimelineItem`]s, to
    /// prevent losing details when a local echo is replaced by its remote
    /// echo.
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
    pub async fn fetch_event_details(&self, event_id: &EventId) -> Result<()> {
        let (index, item) = rfind_event_by_id(&self.inner.items(), event_id)
            .and_then(|(pos, item)| item.as_remote().map(|item| (pos, item.clone())))
            .ok_or(Error::RemoteEventNotInTimeline)?;

        self.inner.fetch_in_reply_to_details(index, item).await?;

        Ok(())
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
        self.inner.set_sender_profiles_pending();
        match self.room().ensure_members().await {
            Ok(_) => {
                self.inner.update_sender_profiles().await;
            }
            Err(e) => {
                self.inner.set_sender_profiles_error(Arc::new(e));
            }
        }
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

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
fn rfind_event_item(
    items: &[Arc<TimelineItem>],
    mut f: impl FnMut(&EventTimelineItem) -> bool,
) -> Option<(usize, &EventTimelineItem)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
        .rfind(|(_, it)| f(it))
}

fn rfind_event_by_id<'a>(
    items: &'a [Arc<TimelineItem>],
    event_id: &EventId,
) -> Option<(usize, &'a EventTimelineItem)> {
    rfind_event_item(items, |it| it.event_id() == Some(event_id))
}

fn find_read_marker(items: &[Arc<TimelineItem>]) -> Option<usize> {
    items.iter().rposition(|item| item.is_read_marker())
}

/// Errors specific to the timeline.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The requested event with a remote echo is not in the timeline.
    #[error("Event with remote echo not found in timeline")]
    RemoteEventNotInTimeline,

    /// The event is currently unsupported for this use case.
    #[error("Unsupported event")]
    UnsupportedEvent,
}
