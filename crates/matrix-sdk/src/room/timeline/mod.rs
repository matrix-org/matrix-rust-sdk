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
use matrix_sdk_base::{
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent},
    locks::Mutex,
};
use ruma::{
    assign,
    events::{fully_read::FullyReadEventContent, AnyMessageLikeEventContent},
    EventId, TransactionId, UInt,
};
use tracing::{error, instrument};

use super::{Joined, Room};
use crate::{
    event_handler::EventHandlerHandle,
    room::{self, MessagesOptions},
    Result,
};

mod event_handler;
mod event_item;
mod inner;
#[cfg(test)]
mod tests;
#[cfg(feature = "e2e-encryption")]
mod to_device;
mod virtual_item;

pub use self::{
    event_item::{
        EncryptedMessage, EventTimelineItem, Message, ReactionDetails, Sticker, TimelineDetails,
        TimelineItemContent, TimelineKey,
    },
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
    inner: Arc<TimelineInner>,
    room: room::Common,
    start_token: Mutex<Option<String>>,
    _end_token: Mutex<Option<String>>,
    event_handler_handles: Vec<EventHandlerHandle>,
}

impl Drop for Timeline {
    fn drop(&mut self) {
        for handle in self.event_handler_handles.drain(..) {
            self.room.client.remove_event_handler(handle);
        }
    }
}

impl Timeline {
    pub(super) async fn new(room: &room::Common) -> Self {
        Self::with_events(room, None, Vec::new())
    }

    pub(crate) fn with_events(
        room: &room::Common,
        prev_token: Option<String>,
        events: Vec<SyncTimelineEvent>,
    ) -> Self {
        let mut inner = TimelineInner::default();
        inner.add_initial_events(events, room.own_user_id());

        let inner = Arc::new(inner);

        let timeline_event_handle = room.add_event_handler({
            let inner = inner.clone();
            move |event, encryption_info: Option<EncryptionInfo>, room: Room| {
                let inner = inner.clone();
                async move {
                    inner.handle_live_event(event, encryption_info, room.own_user_id()).await;
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
            room: room.clone(),
            start_token: Mutex::new(prev_token),
            _end_token: Mutex::new(None),
            event_handler_handles,
        }
    }

    /// Enable tracking of the fully-read marker on this `Timeline`.
    pub async fn with_fully_read_tracking(mut self) -> Self {
        match self.room.account_data_static::<FullyReadEventContent>().await {
            Ok(Some(fully_read)) => match fully_read.deserialize() {
                Ok(fully_read) => {
                    self.inner.set_fully_read_event(fully_read.content.event_id).await
                }
                Err(error) => {
                    error!(?error, "Failed to deserialize `m.fully_read` account data")
                }
            },
            Err(error) => {
                error!(?error, "Failed to get `m.fully_read` account data from the store")
            }
            _ => {}
        }

        let inner = self.inner.clone();
        let fully_read_handle = self.room.add_event_handler(move |event| {
            let inner = inner.clone();
            async move {
                inner.handle_fully_read(event).await;
            }
        });
        self.event_handler_handles.push(fully_read_handle);

        self
    }

    /// Add more events to the start of the timeline.
    #[instrument(skip(self), fields(room_id = %self.room.room_id()))]
    pub async fn paginate_backwards(&self, limit: UInt) -> Result<PaginationOutcome> {
        let mut start_lock = self.start_token.lock().await;
        self.inner.add_loading_indicator();
        let messages = self
            .room
            .messages(assign!(MessagesOptions::backward(), {
                from: start_lock.clone(),
                limit,
            }))
            .await?;

        let own_user_id = self.room.own_user_id();
        let mut num_updates = 0;
        for room_ev in messages.chunk {
            num_updates += self.inner.handle_back_paginated_event(room_ev, own_user_id).await;
        }

        self.inner.remove_loading_indicator();
        let outcome = PaginationOutcome { more_messages: messages.end.is_some(), num_updates };
        *start_lock = messages.end;

        Ok(outcome)
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
                self.room.room_id(),
                self.room.client.olm_machine().expect("Olm machine wasn't started"),
                session_ids.into_iter().map(AsRef::as_ref).collect(),
                self.room.own_user_id(),
            )
            .await;
    }

    /// Get the latest of the timeline's event items.
    pub fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner.items.lock_ref().last()?.as_event().cloned()
    }

    /// Get a signal of the timeline's items.
    ///
    /// You can poll this signal to receive updates, the first of which will
    /// be the full list of items currently available.
    ///
    /// See [`SignalVecExt`](futures_signals::signal_vec::SignalVecExt) for a
    /// high-level API on top of [`SignalVec`].
    pub fn signal(&self) -> impl SignalVec<Item = Arc<TimelineItem>> {
        self.inner.items.signal_vec_cloned()
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
    #[instrument(skip(self, content), fields(room_id = %self.room.room_id()))]
    pub async fn send(
        &self,
        content: AnyMessageLikeEventContent,
        txn_id: Option<&TransactionId>,
    ) -> Result<()> {
        let txn_id = txn_id.map_or_else(TransactionId::new, ToOwned::to_owned);
        self.inner
            .handle_local_event(txn_id.clone(), content.clone(), self.room.own_user_id())
            .await;

        // If this room isn't actually in joined state, we'll get a server error.
        // Not ideal, but works for now.
        let room = Joined { inner: self.room.clone() };

        let response = room.send(content, Some(&txn_id)).await?;
        self.inner.add_event_id(&txn_id, response.event_id);

        Ok(())
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

    fn loading_indicator() -> Self {
        Self::Virtual(VirtualTimelineItem::LoadingIndicator)
    }

    fn is_loading_indicator(&self) -> bool {
        matches!(self, Self::Virtual(VirtualTimelineItem::LoadingIndicator))
    }
}

/// The result of a successful pagination request.
#[derive(Debug)]
// TODO: non-exhaustive breaks UniFFI bridge
//#[non_exhaustive]
pub struct PaginationOutcome {
    /// Whether there's more messages to be paginated.
    pub more_messages: bool,

    /// The number of timeline updates to expect from this pagination.
    ///
    /// Since timeline updates are received asynchronously, you can use this
    /// number to determine whether all updates have been observed, and whether
    /// another back pagination request should be made.
    pub num_updates: u16,
}

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
fn find_event_by_id<'a>(
    lock: &'a [Arc<TimelineItem>],
    event_id: &EventId,
) -> Option<(usize, &'a EventTimelineItem)> {
    lock.iter()
        .enumerate()
        .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
        .rfind(|(_, it)| it.event_id() == Some(event_id))
}

fn find_event_by_txn_id<'a>(
    lock: &'a [Arc<TimelineItem>],
    txn_id: &TransactionId,
) -> Option<(usize, &'a EventTimelineItem)> {
    lock.iter()
        .enumerate()
        .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
        .rfind(|(_, it)| it.key == *txn_id)
}

fn find_read_marker(lock: &[Arc<TimelineItem>]) -> Option<usize> {
    lock.iter()
        .enumerate()
        .rfind(|(_, item)| {
            item.as_virtual().filter(|v| matches!(v, VirtualTimelineItem::ReadMarker)).is_some()
        })
        .map(|(idx, _)| idx)
}
