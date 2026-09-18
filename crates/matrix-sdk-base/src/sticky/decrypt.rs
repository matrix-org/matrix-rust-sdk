// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Decryption of encrypted sticky events.
//!
//! The key of an encrypted sticky event (its type and `content.sticky_key`)
//! lives in the encrypted content, so the event can only enter the map once it
//! is decrypted. Until then it is kept aside in its room's [`StickyEvents`],
//! and retried by the redecryptor when a room key for it arrives.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use futures_util::{Stream, StreamExt as _};
use matrix_sdk_common::{
    deserialized_responses::TimelineEvent,
    executor::{AbortOnDrop, JoinHandleExt as _, spawn},
};
use matrix_sdk_crypto::{DecryptionSettings, OlmMachine, store::types::RoomKeyInfo};
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};

use super::{Candidate, PendingEvent, StickyEvents, now_ms, resolve};
use crate::{response_processors::e2ee, store::BaseStateStore};

/// The outcome of trying to decrypt an encrypted sticky event.
pub(crate) enum Decryption {
    /// The event was decrypted and is ready for the map.
    Resolved(Candidate),
    /// The event couldn't be decrypted (yet).
    Pending(PendingEvent),
    /// The event was decrypted but has no sticky key: it is not tracked.
    Dropped,
}

/// Try to decrypt `pending`, an encrypted sticky event of `room_id`.
pub(crate) async fn decrypt(
    e2ee: &e2ee::E2EE<'_>,
    room_id: &RoomId,
    pending: PendingEvent,
) -> Decryption {
    let event = TimelineEvent::from_plaintext(pending.event.clone());

    match e2ee::decrypt::sync_timeline_event(e2ee, &event, room_id).await {
        Ok(Some(decrypted)) if !decrypted.kind.is_utd() => {
            match resolve(pending.meta, decrypted.kind) {
                Some(candidate) => Decryption::Resolved(candidate),
                None => Decryption::Dropped,
            }
        }
        // Unable to decrypt, or no `OlmMachine` at all.
        Ok(_) => Decryption::Pending(pending),
        Err(error) => {
            warn!(?error, event_id = %pending.meta.event_id, "Failed to decrypt a sticky event");
            Decryption::Pending(pending)
        }
    }
}

/// Retry decrypting the pending sticky events of a room that were encrypted
/// with one of `session_ids`, or all of them if `None`.
pub(crate) async fn retry_pending(
    sticky: &StickyEvents,
    e2ee: &e2ee::E2EE<'_>,
    session_ids: Option<&BTreeSet<String>>,
) {
    let pending = sticky.take_pending(session_ids);

    if pending.is_empty() {
        return;
    }

    trace!(room_id = %sticky.room_id(), count = pending.len(), "Retrying to decrypt sticky events");

    let mut resolved = Vec::new();
    let mut still_pending = Vec::new();

    for event in pending {
        match decrypt(e2ee, sticky.room_id(), event).await {
            Decryption::Resolved(candidate) => resolved.push(candidate),
            Decryption::Pending(event) => still_pending.push(event),
            Decryption::Dropped => {}
        }
    }

    sticky.ingest(now_ms(), resolved);
    sticky.park(still_pending);
}

/// Spawn the task that retries pending sticky events as room keys arrive on
/// `room_keys_stream` (the [`room_keys_received_stream`] of the `OlmMachine`
/// behind `olm_machine`).
///
/// [`room_keys_received_stream`]: matrix_sdk_crypto::store::CryptoStoreWrapper::room_keys_received_stream
pub(crate) fn spawn_redecryptor<S, E>(
    room_keys_stream: S,
    olm_machine: Arc<RwLock<Option<OlmMachine>>>,
    decryption_settings: DecryptionSettings,
    state_store: BaseStateStore,
) -> AbortOnDrop<()>
where
    S: Stream<Item = Result<Vec<RoomKeyInfo>, E>> + Send + 'static,
    E: Send + 'static,
{
    spawn(run_redecryptor(room_keys_stream, olm_machine, decryption_settings, state_store))
        .abort_on_drop()
}

async fn run_redecryptor<S, E>(
    room_keys_stream: S,
    olm_machine: Arc<RwLock<Option<OlmMachine>>>,
    decryption_settings: DecryptionSettings,
    state_store: BaseStateStore,
) where
    S: Stream<Item = Result<Vec<RoomKeyInfo>, E>>,
{
    let mut room_keys_stream = std::pin::pin!(room_keys_stream);

    while let Some(item) = room_keys_stream.next().await {
        // Which pending events to retry, per room: those encrypted with the
        // sessions we just received keys for, or, if we missed some keys, every
        // pending event of every room.
        let rooms: Vec<(OwnedRoomId, Option<BTreeSet<String>>)> = match item {
            Ok(room_keys) => {
                let mut sessions_per_room: BTreeMap<OwnedRoomId, BTreeSet<String>> =
                    BTreeMap::new();

                for key in room_keys {
                    sessions_per_room.entry(key.room_id).or_default().insert(key.session_id);
                }

                sessions_per_room
                    .into_iter()
                    .map(|(room_id, session_ids)| (room_id, Some(session_ids)))
                    .collect()
            }
            Err(_) => {
                debug!("The room keys stream lagged, retrying every pending sticky event");

                state_store
                    .rooms()
                    .into_iter()
                    .map(|room| (room.room_id().to_owned(), None))
                    .collect()
            }
        };

        for (room_id, session_ids) in rooms {
            let Some(room) = state_store.room(&room_id) else { continue };
            let Some(sticky) = room.sticky_events_if_any() else { continue };

            if !sticky.has_pending() {
                continue;
            }

            let olm_machine = olm_machine.read().await;
            let e2ee = e2ee::E2EE::new(olm_machine.as_ref(), &decryption_settings, false);

            retry_pending(sticky, &e2ee, session_ids.as_ref()).await;
        }
    }
}
