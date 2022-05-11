use std::{fmt, pin::Pin, sync::Arc};

use dashmap::DashSet;
use futures_channel::mpsc;
use futures_core::{
    stream::{BoxStream, Stream},
    task::{Context, Poll},
};
use ruma::OwnedEventId;
use thiserror::Error;
use tracing::trace;

use crate::{
    deserialized_responses::{SyncRoomEvent, TimelineSlice},
    store::Result,
};

const CHANNEL_LIMIT: usize = 10;

/// Errors in a timeline stream
#[derive(Error, Debug)]
pub enum TimelineStreamError {
    /// The end of the stored timeline was reached.
    #[error("the end of the stored timeline was reached")]
    EndCache {
        /// The given token should be used to request more events from the
        /// server.
        fetch_more_token: String,
    },
    /// The event in the store produced an error
    #[error("the event in the store produced an error")]
    Store(crate::StoreError),
}

/// A `Stream` of timeline of a room
pub struct TimelineStreamBackward<'a> {
    receiver: mpsc::Receiver<TimelineSlice>,
    stored_events: Option<BoxStream<'a, Result<SyncRoomEvent>>>,
    pending: Vec<SyncRoomEvent>,
    event_ids: Arc<DashSet<OwnedEventId>>,
    token: Option<String>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineStreamBackward<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimelineStream")
            .field("event_ids", &self.event_ids)
            .field("token", &self.token)
            .field("pending", &self.pending)
            .finish()
    }
}

impl<'a> TimelineStreamBackward<'a> {
    /// Creates a tuple containing `Self` and a `Sender` to send new events to
    /// this stream
    ///
    /// The stream is terminated when the entire timeline was returned,
    /// otherwise an error will be returned till the issue is resolved, aka
    /// new messages are loaded.
    ///
    /// # Arguments
    ///
    /// * `event_ids` - A set to store `EventId`s already seen by the stream.
    ///   This should be shared
    /// between the `TimelineStreamBackward` and `TimelineStreamForward` stream.
    ///
    /// * `token` - The current sync token or the token that identifies the end
    ///   of the `stored_events`.
    ///
    /// * `stored_events` - A stream of events that are currently stored
    ///   locally.
    pub(crate) fn new(
        event_ids: Arc<DashSet<OwnedEventId>>,
        token: Option<String>,
        stored_events: Option<BoxStream<'a, Result<SyncRoomEvent>>>,
    ) -> (Self, mpsc::Sender<TimelineSlice>) {
        let (sender, receiver) = mpsc::channel(CHANNEL_LIMIT);
        let self_ = Self { event_ids, pending: Vec::new(), stored_events, token, receiver };

        (self_, sender)
    }

    fn handle_new_slice(
        &mut self,
        slice: TimelineSlice,
    ) -> Poll<Option<Result<SyncRoomEvent, TimelineStreamError>>> {
        // Check if this is the batch we are expecting
        if self.token.is_some() && self.token != Some(slice.start) {
            trace!("Store received a timeline batch that wasn't expected");
            return Poll::Pending;
        }

        // There is a gap in the timeline. Therefore, terminate the stream.
        if slice.limited {
            return Poll::Ready(None);
        }

        // The end of the timeline was reached
        if slice.events.is_empty() {
            return Poll::Ready(None);
        }

        for event in slice.events.into_iter().rev().filter(|event| {
            self.event_ids
                .insert(event.event_id().expect("Timeline events always have an event id."))
        }) {
            self.pending.push(event);
        }
        self.token = slice.end;

        if let Some(event) = self.pending.pop() {
            Poll::Ready(Some(Ok(event)))
        } else if let Some(token) = &self.token {
            Poll::Ready(Some(Err(TimelineStreamError::EndCache {
                fetch_more_token: token.to_string(),
            })))
        } else {
            Poll::Ready(None)
        }
    }
}

impl<'a> Stream for TimelineStreamBackward<'a> {
    type Item = Result<SyncRoomEvent, TimelineStreamError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(stored_events) = &mut this.stored_events {
            match Pin::new(stored_events).poll_next(cx) {
                Poll::Ready(None) => {}
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(event)) => {
                    return Poll::Ready(Some(event.map_err(TimelineStreamError::Store)))
                }
            }

            // We returned all events the store gave to us
            this.stored_events = None;
        }

        if let Some(event) = this.pending.pop() {
            Poll::Ready(Some(Ok(event)))
        } else {
            loop {
                match Pin::new(&mut this.receiver).poll_next(cx) {
                    Poll::Ready(Some(slice)) => match this.handle_new_slice(slice) {
                        Poll::Pending => continue,
                        other => break other,
                    },
                    Poll::Ready(None) => break Poll::Ready(None),
                    Poll::Pending => {
                        if let Some(token) = &this.token {
                            // We tell the consumer that there are no more evens in cache
                            break Poll::Ready(Some(Err(TimelineStreamError::EndCache {
                                fetch_more_token: token.to_string(),
                            })));
                        } else {
                            break Poll::Ready(None);
                        }
                    }
                };
            }
        }
    }
}

/// A `Stream` of timeline of a room
pub struct TimelineStreamForward {
    receiver: mpsc::Receiver<TimelineSlice>,
    pending: Vec<SyncRoomEvent>,
    event_ids: Arc<DashSet<OwnedEventId>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineStreamForward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimelineStream")
            .field("event_ids", &self.event_ids)
            .field("pending", &self.pending)
            .finish()
    }
}

impl TimelineStreamForward {
    /// Creates a tuple containing `Self` and a `Sender` to send new events to
    /// this stream
    ///
    /// The stream is only terminated when there would be a gap in the timeline.
    /// This happens when a `SyncResponse` is obtained with a limited
    /// timeline.
    ///
    /// # Arguments
    ///
    /// * `event_ids` - A set to store `EventId`s already seen by the stream.
    ///   This should be shared
    /// between the `TimelineStreamBackward` and `TimelineStreamForward` stream.
    pub(crate) fn new(
        event_ids: Arc<DashSet<OwnedEventId>>,
    ) -> (Self, mpsc::Sender<TimelineSlice>) {
        let (sender, receiver) = mpsc::channel(CHANNEL_LIMIT);
        let self_ = Self { event_ids, pending: Vec::new(), receiver };

        (self_, sender)
    }

    fn handle_new_slice(&mut self, slice: TimelineSlice) -> Poll<Option<SyncRoomEvent>> {
        // There is a gap in the timeline. Therefore, terminate the stream.
        if slice.limited {
            return Poll::Ready(None);
        }

        for event in slice.events.into_iter().rev().filter(|event| {
            self.event_ids
                .insert(event.event_id().expect("Timeline events always have an event id."))
        }) {
            self.pending.push(event);
        }

        if let Some(event) = self.pending.pop() {
            Poll::Ready(Some(event))
        } else {
            Poll::Pending
        }
    }
}

impl Stream for TimelineStreamForward {
    type Item = SyncRoomEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(event) = this.pending.pop() {
            Poll::Ready(Some(event))
        } else {
            loop {
                match Pin::new(&mut this.receiver).poll_next(cx) {
                    Poll::Ready(Some(slice)) => match this.handle_new_slice(slice) {
                        Poll::Pending => continue,
                        other => break other,
                    },
                    Poll::Ready(None) => break Poll::Ready(None),
                    Poll::Pending => break Poll::Pending,
                }
            }
        }
    }
}
