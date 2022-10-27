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
    sync::{Arc, Mutex},
};

use futures_core::Stream;
use futures_signals::signal_vec::{MutableVec, SignalVec, SignalVecExt, VecDiff};
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    assign,
    events::{reaction::Relation as AnnotationRelation, AnyMessageLikeEventContent},
    OwnedEventId, OwnedUserId, TransactionId, UInt,
};
use tracing::{error, instrument, warn};

use super::{Joined, Room};
use crate::{
    event_handler::EventHandlerDropGuard,
    room::{self, MessagesOptions},
    Result,
};

mod event_handler;
mod event_item;
mod virtual_item;

pub use self::{
    event_item::{
        EventTimelineItem, Message, PaginationOutcome, ReactionDetails, TimelineDetails,
        TimelineItemContent, TimelineKey,
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
    inner: TimelineInner,
    room: room::Common,
    start_token: Mutex<Option<String>>,
    _end_token: Mutex<Option<String>>,
    _event_handler_guard: EventHandlerDropGuard,
}

#[derive(Clone, Debug, Default)]
struct TimelineInner {
    items: MutableVec<Arc<TimelineItem>>,
    // Reaction event / txn ID => sender and reaction data
    reaction_map: Arc<Mutex<HashMap<TimelineKey, (OwnedUserId, AnnotationRelation)>>>,
}

impl Timeline {
    pub(super) fn new(room: &room::Common) -> Self {
        let inner = TimelineInner::default();

        let handle = room.add_event_handler({
            let inner = inner.clone();
            move |event, encryption_info: Option<EncryptionInfo>, room: Room| {
                let inner = inner.clone();
                async move {
                    inner.handle_live_event(event, encryption_info, room.own_user_id());
                }
            }
        });
        let _event_handler_guard = room.client.event_handler_drop_guard(handle);

        Timeline {
            inner,
            room: room.clone(),
            start_token: Mutex::new(None),
            _end_token: Mutex::new(None),
            _event_handler_guard,
        }
    }

    /// Add more events to the start of the timeline.
    #[instrument(skip(self), fields(room_id = %self.room.room_id()))]
    pub async fn paginate_backwards(&self, limit: UInt) -> Result<PaginationOutcome> {
        let start = self.start_token.lock().unwrap().clone();
        let messages = self
            .room
            .messages(assign!(MessagesOptions::backward(), {
                from: start.as_deref(),
                limit,
            }))
            .await?;

        let outcome = PaginationOutcome { more_messages: messages.end.is_some() };
        *self.start_token.lock().unwrap() = messages.end;

        let own_user_id = self.room.own_user_id();
        for room_ev in messages.chunk {
            self.inner.handle_back_paginated_event(
                room_ev.event.cast(),
                room_ev.encryption_info,
                own_user_id,
            );
        }

        Ok(outcome)
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
        self.inner.handle_local_event(txn_id.clone(), content.clone(), self.room.own_user_id());

        // If this room isn't actually in joined state, we'll get a server error.
        // Not ideal, but works for now.
        let room = Joined { inner: self.room.clone() };

        let response = room.send(content, Some(&txn_id)).await?;
        add_event_id(&self.inner, &txn_id, response.event_id);

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

fn add_event_id(items: &TimelineInner, txn_id: &TransactionId, event_id: OwnedEventId) {
    let mut lock = items.items.lock_mut();
    if let Some((idx, item)) = find_event(&lock, txn_id) {
        match &item.key {
            TimelineKey::TransactionId(_) => {
                lock.set_cloned(
                    idx,
                    Arc::new(TimelineItem::Event(item.with_event_id(Some(event_id)))),
                );
            }
            TimelineKey::EventId(ev_id) => {
                if *ev_id != event_id {
                    error!("remote echo and send-event response disagree on the event ID");
                }
            }
        }
    } else {
        warn!(%txn_id, "Timeline item not found, can't add event ID");
    }
}
