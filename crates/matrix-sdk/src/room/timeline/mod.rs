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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};

use futures_core::Stream;
use futures_signals::signal_vec::{SignalVec, SignalVecExt, VecDiff};
use matrix_sdk_base::deserialized_responses::{EncryptionInfo, SyncTimelineEvent};
use ruma::{
    assign,
    events::{fully_read::FullyReadEventContent, relation::Annotation, AnyMessageLikeEventContent},
    OwnedEventId, OwnedUserId, TransactionId, UInt,
};
use tracing::{error, instrument};

use super::{Joined, Room};
use crate::{
    event_handler::EventHandlerDropGuard,
    room::{self, MessagesOptions},
    Result,
};

mod event_handler;
mod event_item;
mod inner;
#[cfg(test)]
mod tests;
mod virtual_item;

use self::inner::TimelineInner;
pub use self::{
    event_item::{
        EncryptedMessage, EventTimelineItem, Message, PaginationOutcome, ReactionDetails,
        TimelineDetails, TimelineItemContent, TimelineKey,
    },
    virtual_item::VirtualTimelineItem,
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
    start_token: StdMutex<Option<String>>,
    _end_token: StdMutex<Option<String>>,
    _timeline_event_handler_guard: EventHandlerDropGuard,
    _fully_read_handler_guard: Option<EventHandlerDropGuard>,
    #[cfg(feature = "e2e-encryption")]
    _room_key_handler_guard: EventHandlerDropGuard,
}

/// Non-signalling parts of `TimelineInner`.
#[derive(Debug, Default)]
struct TimelineInnerMetadata {
    // Reaction event / txn ID => sender and reaction data
    reaction_map: HashMap<TimelineKey, (OwnedUserId, Annotation)>,
    fully_read_event: Option<OwnedEventId>,
    fully_read_event_in_timeline: bool,
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
        let _timeline_event_handler_guard =
            room.client.event_handler_drop_guard(timeline_event_handle);

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.
        #[cfg(feature = "e2e-encryption")]
        let room_id = room.room_id().to_owned();
        #[cfg(feature = "e2e-encryption")]
        let room_key_handle = room.client.add_event_handler({
            use std::iter;

            use ruma::events::room_key::ToDeviceRoomKeyEvent;

            use crate::Client;

            let inner = inner.clone();
            move |event: ToDeviceRoomKeyEvent, client: Client| {
                let inner = inner.clone();
                let room_id = room_id.clone();
                async move {
                    if event.content.room_id != room_id {
                        return;
                    }

                    let Some(olm_machine) = client.olm_machine() else {
                        error!("The olm machine isn't yet available");
                        return;
                    };

                    let session_id = event.content.session_id;
                    let Some(own_user_id) = client.user_id() else {
                        error!("The user's own ID isn't available");
                        return;
                    };

                    inner
                        .retry_event_decryption(
                            &room_id,
                            olm_machine,
                            iter::once(session_id.as_str()).collect(),
                            own_user_id,
                        )
                        .await;
                }
            }
        });
        #[cfg(feature = "e2e-encryption")]
        let _room_key_handler_guard = room.client.event_handler_drop_guard(room_key_handle);

        Timeline {
            inner,
            room: room.clone(),
            start_token: StdMutex::new(prev_token),
            _end_token: StdMutex::new(None),
            _timeline_event_handler_guard,
            _fully_read_handler_guard: None,
            #[cfg(feature = "e2e-encryption")]
            _room_key_handler_guard,
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
        self._fully_read_handler_guard =
            Some(self.room.client.event_handler_drop_guard(fully_read_handle));

        self
    }

    /// Add more events to the start of the timeline.
    #[instrument(skip(self), fields(room_id = %self.room.room_id()))]
    pub async fn paginate_backwards(&self, limit: UInt) -> Result<PaginationOutcome> {
        let start = self.start_token.lock().unwrap().clone();
        let messages = self
            .room
            .messages(assign!(MessagesOptions::backward(), {
                from: start,
                limit,
            }))
            .await?;

        let outcome = PaginationOutcome { more_messages: messages.end.is_some() };
        *self.start_token.lock().unwrap() = messages.end;

        let own_user_id = self.room.own_user_id();
        for room_ev in messages.chunk {
            self.inner.handle_back_paginated_event(room_ev, own_user_id).await;
        }

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
}

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
fn find_event(
    lock: &[Arc<TimelineItem>],
    key: impl PartialEq<TimelineKey>,
) -> Option<(usize, &EventTimelineItem)> {
    lock.iter()
        .enumerate()
        .filter_map(|(idx, item)| Some((idx, item.as_event()?)))
        .rfind(|(_, it)| key == it.key)
}

fn find_read_marker(lock: &[Arc<TimelineItem>]) -> Option<usize> {
    lock.iter()
        .enumerate()
        .rfind(|(_, item)| {
            item.as_virtual().filter(|v| matches!(v, VirtualTimelineItem::ReadMarker)).is_some()
        })
        .map(|(idx, _)| idx)
}
