//! Matrix driver implementation that exposes Matrix functionality
//! that is relevant for the widget API.

use ruma::{
    api::client::{
        account::request_openid_token::v3::{Request as OpenIdRequest, Response as OpenIdResponse},
        filter::RoomEventFilter,
    },
    assign,
    events::{AnySyncTimelineEvent, AnyTimelineEvent, TimelineEventType},
    serde::Raw,
    OwnedEventId,
};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::{
    event_handler::EventHandlerDropGuard, room::MessagesOptions, HttpResult, Result, Room,
};

/// Thin wrapper around the Matrix API that provides convenient high level
/// functions.
pub struct MatrixDriver {
    room: Room,
}

impl MatrixDriver {
    /// Creates a new `MatrixDriver` for a given `room`.
    pub fn new(room: Room) -> Self {
        Self { room }
    }

    /// Requests an OpenID token for the current user.
    pub async fn get_open_id(&self) -> HttpResult<OpenIdResponse> {
        let user_id = self.room.own_user_id().to_owned();
        self.room.client.send(OpenIdRequest::new(user_id), None).await
    }

    /// Reads the latest `limit` events of a given `event_type` from the room.
    pub async fn read(
        &self,
        event_type: TimelineEventType,
        limit: u32,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let options = assign!(MessagesOptions::backward(), {
            limit: limit.into(),
            filter: assign!(RoomEventFilter::default(), {
                types: Some(vec![event_type.to_string()])
            }),
        });

        let messages = self.room.messages(options).await?;
        Ok(messages.chunk.into_iter().map(|ev| ev.event.cast()).collect())
    }

    /// Sends a given `event` to the room.
    pub async fn send(
        &self,
        event_type: TimelineEventType,
        state_key: Option<String>,
        content: JsonValue,
    ) -> Result<OwnedEventId> {
        let type_str = event_type.to_string();
        Ok(match state_key {
            Some(key) => self.room.send_state_event_raw(content, &type_str, &key).await?.event_id,
            None => self.room.send_raw(content, &type_str, None).await?.event_id,
        })
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub fn events(&self) -> EventReceiver {
        let (tx, rx) = unbounded_channel();
        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(raw.cast());
            async {}
        });

        let drop_guard = self.room.client().event_handler_drop_guard(handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub struct EventReceiver {
    rx: UnboundedReceiver<Raw<AnyTimelineEvent>>,
    _drop_guard: EventHandlerDropGuard,
}

impl EventReceiver {
    pub async fn recv(&mut self) -> Option<Raw<AnyTimelineEvent>> {
        self.rx.recv().await
    }
}
