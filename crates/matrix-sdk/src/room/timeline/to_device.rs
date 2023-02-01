use std::{iter, sync::Arc};

use ruma::{
    events::{forwarded_room_key::ToDeviceForwardedRoomKeyEvent, room_key::ToDeviceRoomKeyEvent},
    OwnedRoomId,
};
use tracing::{debug_span, error, trace, Instrument};

use super::inner::TimelineInner;
use crate::{event_handler::EventHandler, Client};

pub(super) fn handle_room_key_event(
    inner: Arc<TimelineInner>,
    room_id: OwnedRoomId,
) -> impl EventHandler<ToDeviceRoomKeyEvent, (Client,)> {
    move |event: ToDeviceRoomKeyEvent, client: Client| {
        let inner = inner.clone();
        let room_id = room_id.clone();
        async move {
            let event_room_id = event.content.room_id;
            let session_id = event.content.session_id;
            retry_decryption(client, inner, room_id, event_room_id, session_id).await;
        }
        .instrument(debug_span!("handle_room_key_event"))
    }
}

pub(super) fn handle_forwarded_room_key_event(
    inner: Arc<TimelineInner>,
    room_id: OwnedRoomId,
) -> impl EventHandler<ToDeviceForwardedRoomKeyEvent, (Client,)> {
    move |event: ToDeviceForwardedRoomKeyEvent, client: Client| {
        let inner = inner.clone();
        let room_id = room_id.clone();
        async move {
            let event_room_id = event.content.room_id;
            let session_id = event.content.session_id;
            retry_decryption(client, inner, room_id, event_room_id, session_id).await;
        }
        .instrument(debug_span!("handle_forwarded_room_key_event"))
    }
}

async fn retry_decryption(
    client: Client,
    inner: Arc<TimelineInner>,
    room_id: OwnedRoomId,
    event_room_id: OwnedRoomId,
    session_id: String,
) {
    if event_room_id != room_id {
        trace!(
            ?event_room_id, timeline_room_id = ?room_id, ?session_id,
            "Received to-device room key event for a different room, ignoring"
        );
        return;
    }

    let Some(olm_machine) = client.olm_machine() else {
        error!("The olm machine isn't yet available");
        return;
    };

    inner
        .retry_event_decryption(
            &room_id,
            olm_machine,
            Some(iter::once(session_id.as_str()).collect()),
        )
        .await;
}
