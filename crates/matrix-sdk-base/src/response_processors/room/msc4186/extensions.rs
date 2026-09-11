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

use std::collections::BTreeMap;
#[cfg(feature = "unstable-msc4354")]
use std::collections::BTreeSet;

#[cfg(feature = "unstable-msc4354")]
use matrix_sdk_common::deserialized_responses::{TimelineEvent, TimelineEventKind};
#[cfg(feature = "unstable-msc4354")]
use ruma::events::AnySyncTimelineEvent;
use ruma::{
    OwnedRoomId, RoomId,
    api::client::sync::sync_events::v5 as http,
    events::{AnySyncEphemeralRoomEvent, SyncEphemeralRoomEvent, receipt::ReceiptEventContent},
    serde::Raw,
};
#[cfg(feature = "unstable-msc4354")]
use tracing::trace;

#[cfg(all(feature = "unstable-msc4354", feature = "e2e-encryption"))]
use super::super::super::e2ee;
use super::super::super::{
    Context, account_data::for_room as account_data_for_room, ephemeral_events::dispatch_receipt,
};
#[cfg(feature = "unstable-msc4354")]
use crate::sticky::{Candidate, Payload, PendingEvent, classify, now_ms, resolve};
#[cfg(all(feature = "unstable-msc4354", feature = "e2e-encryption"))]
use crate::sticky::{Decryption, decrypt};
use crate::{
    RoomState,
    store::BaseStateStore,
    sync::{JoinedRoomUpdate, RoomUpdates},
};

/// Dispatch the ephemeral events in the `extensions.typing` part of the
/// response.
pub fn dispatch_typing_ephemeral_events(
    typing: &http::response::Typing,
    joined_room_updates: &mut BTreeMap<OwnedRoomId, JoinedRoomUpdate>,
) {
    for (room_id, raw) in &typing.rooms {
        joined_room_updates
            .entry(room_id.to_owned())
            .or_default()
            .ephemeral
            .push(raw.clone().cast());
    }
}

/// Dispatch the ephemeral event in the `extensions.receipts` part of the
/// response for a particular room.
pub fn dispatch_receipt_ephemeral_event_for_room(
    context: &mut Context,
    room_id: &RoomId,
    receipt: &Raw<SyncEphemeralRoomEvent<ReceiptEventContent>>,
) {
    let receipt: Raw<AnySyncEphemeralRoomEvent> = receipt.cast_ref().clone();

    dispatch_receipt(context, &receipt, room_id);
}

pub fn room_account_data(
    context: &mut Context,
    account_data: &http::response::AccountData,
    room_updates: &mut RoomUpdates,
    state_store: &BaseStateStore,
) {
    for (room_id, raw) in &account_data.rooms {
        account_data_for_room(context, room_id, raw, state_store);

        if let Some(room) = state_store.room(room_id) {
            match room.state() {
                RoomState::Joined => room_updates
                    .joined
                    .entry(room_id.to_owned())
                    .or_default()
                    .account_data
                    .append(&mut raw.to_vec()),
                RoomState::Left | RoomState::Banned => room_updates
                    .left
                    .entry(room_id.to_owned())
                    .or_default()
                    .account_data
                    .append(&mut raw.to_vec()),
                RoomState::Invited | RoomState::Knocked => {}
            }
        }
    }
}

/// Feed the sticky events (MSC4354) of the response into the map of each room.
///
/// Sticky events arrive in the `extensions.sticky_events` part of the
/// response, and in the timeline of the rooms (the server leaves out of the
/// extension the ones that are in a timeline). The timeline events were already
/// decrypted when the rooms were processed, so they are taken from
/// `joined_room_updates` rather than decrypted again; only the events of the
/// extension are decrypted here.
///
/// This must run once the rooms are saved, so that the state of each room is
/// up to date.
#[cfg(feature = "unstable-msc4354")]
pub async fn sticky_events(
    sticky_events: &http::response::StickyEvents,
    rooms: &BTreeMap<OwnedRoomId, http::response::Room>,
    joined_room_updates: &BTreeMap<OwnedRoomId, JoinedRoomUpdate>,
    state_store: &BaseStateStore,
    #[cfg(feature = "e2e-encryption")] e2ee: &e2ee::E2EE<'_>,
) {
    let room_ids: BTreeSet<&RoomId> =
        sticky_events.rooms.keys().chain(rooms.keys()).map(AsRef::as_ref).collect();

    for room_id in room_ids {
        let Some(room) = state_store.room(room_id) else {
            trace!(?room_id, "Ignoring sticky events for an unknown room");
            continue;
        };

        // Sticky events are only meaningful to joined members. Forget what we
        // know about a room we are not in anymore.
        if room.state() != RoomState::Joined {
            if let Some(sticky) = room.sticky_events_if_any() {
                sticky.clear();
            }
            continue;
        }

        let mut collector = StickyEventsCollector {
            now: now_ms(),
            room_id,
            #[cfg(feature = "e2e-encryption")]
            e2ee,
            candidates: Vec::new(),
            pending: Vec::new(),
        };

        // The sticky events of the timeline, paired with their processed (i.e.
        // decrypted) counterparts.
        if let Some(room_response) = rooms.get(room_id) {
            // The processed timeline has one event per raw event, in order. Not
            // having it (or, defensively, having a different number of events)
            // means decrypting the encrypted ones here.
            let processed_events = joined_room_updates
                .get(room_id)
                .map(|update| update.timeline.events.as_slice())
                .filter(|events| events.len() == room_response.timeline.len())
                .unwrap_or_default();

            for (index, raw) in room_response.timeline.iter().enumerate() {
                collector.collect(raw, processed_events.get(index)).await;
            }
        }

        // The sticky events of the extension.
        if let Some(sticky_room) = sticky_events.rooms.get(room_id) {
            for raw in &sticky_room.events {
                collector.collect(raw, None).await;
            }
        }

        let StickyEventsCollector { now, candidates, pending, .. } = collector;

        if candidates.is_empty() && pending.is_empty() {
            continue;
        }

        let sticky = room.sticky_events();
        sticky.ingest(now, candidates);
        sticky.park(pending);
    }
}

/// Gathers the sticky events of a single room out of raw sync events, as map
/// candidates and, for the encrypted ones that can't be decrypted, pending
/// events.
#[cfg(feature = "unstable-msc4354")]
struct StickyEventsCollector<'a> {
    now: u64,
    room_id: &'a RoomId,
    #[cfg(feature = "e2e-encryption")]
    e2ee: &'a e2ee::E2EE<'a>,
    candidates: Vec<Candidate>,
    pending: Vec<PendingEvent>,
}

#[cfg(feature = "unstable-msc4354")]
impl StickyEventsCollector<'_> {
    /// Collect `raw`, if it is a sticky event. `processed` is its already
    /// processed (thus decrypted) form, when it went through the timeline
    /// processing.
    #[cfg_attr(not(feature = "e2e-encryption"), allow(clippy::unused_async))]
    async fn collect(
        &mut self,
        raw: &Raw<AnySyncTimelineEvent>,
        processed: Option<&TimelineEvent>,
    ) {
        let Some(meta) = classify(self.now, raw) else { return };

        match &meta.payload {
            Payload::Plain { .. } => {
                let kind = match processed {
                    Some(processed) => processed.kind.clone(),
                    None => TimelineEventKind::PlainText { event: raw.clone() },
                };
                self.candidates.extend(resolve(meta, kind));
            }

            Payload::Encrypted { .. } => match processed.map(|processed| &processed.kind) {
                // Already decrypted by the timeline processing.
                Some(kind @ TimelineEventKind::Decrypted(_)) => {
                    self.candidates.extend(resolve(meta, kind.clone()));
                }

                // The timeline processing couldn't decrypt it, no point in
                // trying again right away.
                Some(_) => self.pending.push(PendingEvent { meta, event: raw.clone() }),

                // Not processed yet: try to decrypt it.
                None => {
                    let event = PendingEvent { meta, event: raw.clone() };

                    #[cfg(feature = "e2e-encryption")]
                    match decrypt(self.e2ee, self.room_id, event).await {
                        Decryption::Resolved(candidate) => self.candidates.push(candidate),
                        Decryption::Pending(event) => self.pending.push(event),
                        Decryption::Dropped => {}
                    }

                    #[cfg(not(feature = "e2e-encryption"))]
                    {
                        trace!(
                            room_id = %self.room_id,
                            event_id = %event.meta.event_id,
                            "Keeping an encrypted sticky event aside, it can't be decrypted \
                             without encryption support"
                        );
                        self.pending.push(event);
                    }
                }
            },
        }
    }
}
