use std::{iter, sync::Arc};

use ruma::{events::room_key::ToDeviceRoomKeyEvent, OwnedRoomId};
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
            if event.content.room_id != room_id {
                let event_room_id = &event.content.room_id;
                let session_id = &event.content.session_id;
                trace!(
                    %event_room_id, timeline_room_id = %room_id, %session_id,
                    "Received to-device room key event for a different room, ignoring"
                );
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
        .instrument(debug_span!("handle_room_key_event"))
    }
}
