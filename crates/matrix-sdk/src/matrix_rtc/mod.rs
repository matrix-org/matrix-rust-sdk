// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Matrix RTC (Real Time Communication) module that contains facilities to
//! track and manage Matrix RTC calls.

use std::collections::HashMap;

use eyeball::{Observable, Subscriber};
use matrix_sdk_base::{
    sync::Timeline, CallMemberDiff, CallMemberEventExt, CallMemberIdentifier, CallMemberInfo,
    RoomCall,
};
use ruma::{
    api::client::filter::RoomEventFilter,
    assign,
    events::{call::member::OriginalSyncCallMemberEvent, AnySyncStateEvent, StateEventType},
    serde::Raw,
    MilliSecondsSinceUnixEpoch,
};
use tokio::sync::broadcast::Receiver;

use self::find_start::{FindStart, FindState, Mark};
use crate::{room::MessagesOptions, sync::RoomUpdate, Room};

mod find_start;

/// Receives room sync updates and tracks the active call in the room.
/// Returns the observable value (subscriber) so that the caller can observe
/// updates to the active room call. The `sync_updates` can be acquired by
/// calling `Room::subscribe_to_updates()`.
pub(crate) fn monitor_room_call(
    mut sync_updates: Receiver<RoomUpdate>,
) -> Subscriber<Option<RoomCall>> {
    let mut call_observable: Observable<Option<RoomCall>> = Observable::new(None);
    let subscriber = Observable::subscribe(&call_observable);

    tokio::spawn(async move {
        while let Ok(update) = sync_updates.recv().await {
            // We only care about the updates in a joined room.
            let RoomUpdate::Joined { room, updates } = update else {
                Observable::set(&mut call_observable, None);
                continue;
            };

            // Let's see which active members do we have in a call right now.
            let members = active_call_members(updates.state);

            let (has_call, known_call) = (!members.is_empty(), call_observable.as_ref());
            match (has_call, known_call) {
                // If there is no call and there was no call before, nothing to do.
                (false, None) => {}
                // If there is no call and there was a call before, the call has ended.
                (false, Some(..)) => {
                    Observable::set(&mut call_observable, None);
                }
                // If there was a call before.
                (true, known_call) => {
                    // If there was an active call before, we may not need
                    // to search for a call start again.
                    let call_start = known_call.and_then(|call| {
                        // Check if at laest one of the members contained in
                        // the previously known call is still in the call. If
                        // we find a match, then 99% probability that it's the
                        // same call as before, hence we don't need to search
                        // for a call start.
                        let start_known = members
                            .iter()
                            .any(|(id, a)| call.members.get(id).map(|b| a == b).unwrap_or(false));
                        start_known.then_some(call.start_time)
                    });

                    // The only reason I don't use `map()` here is because the `unwrap_or_else()`
                    // closure would need to be async.
                    let start_time = match call_start {
                        Some(start_time) => start_time,
                        None => {
                            let identifiers = members.keys().cloned();
                            let timeline = updates.timeline;
                            let timestamp = find_call_start(identifiers, timeline, room).await;
                            timestamp.unwrap_or_else(|| {
                                tracing::warn!("Failed to find call start time");
                                let earliest = members.values().map(|m| m.member_since).min();
                                earliest.unwrap_or_else(MilliSecondsSinceUnixEpoch::now)
                            })
                        }
                    };

                    // Notify the call update.
                    Observable::set(&mut call_observable, Some(RoomCall { start_time, members }));
                }
            }
        }
    });

    subscriber
}

/// Extracts the active call members from the provided events.
fn active_call_members(
    events: Vec<Raw<AnySyncStateEvent>>,
) -> HashMap<CallMemberIdentifier, CallMemberInfo> {
    events
        .into_iter()
        .filter_map(|event| {
            let AnySyncStateEvent::CallMember(event) = event.deserialize().ok()? else {
                return None;
            };
            event.as_original().cloned()
        })
        .flat_map(|event| {
            event.current_memberships().into_iter().map(move |m| {
                let id = CallMemberIdentifier {
                    user_id: event.state_key.clone(),
                    device_id: m.device_id.clone(),
                };
                (id, m.info)
            })
        })
        .collect()
}

/// Finds the timestamp of the call start by finding the first member who joined
/// the call. Assumes that the provided `members`` are the only members of the
/// call as of "right now", iterates over the the provided `timeline` backwards,
/// performing pagination back on demand via `room` if necessary.
async fn find_call_start(
    members: impl IntoIterator<Item = CallMemberIdentifier>,
    timeline: Timeline,
    room: Room,
) -> Option<MilliSecondsSinceUnixEpoch> {
    let mut search = CallStartFinder::new(members);
    let mut event_source = EventSource::new(timeline, room);
    while let Some(event) = event_source.next().await {
        if let Some(timestamp) = search.process(event) {
            return Some(timestamp);
        }
    }

    None
}

/// Simple state machine that processes call member events until the call start
/// is found.
struct CallStartFinder {
    state: Option<FindStart<CallMemberIdentifier, MilliSecondsSinceUnixEpoch>>,
}

impl CallStartFinder {
    /// Creates a call start finder, assuming that given `members` are the known
    /// members that are currently in the call.
    fn new(members: impl IntoIterator<Item = CallMemberIdentifier>) -> Self {
        let find_start = FindStart::new(members, MilliSecondsSinceUnixEpoch::now());
        Self { state: Some(find_start) }
    }

    /// Processes the next call member events. If `Some(timestamp)` is returned,
    /// then the start of a call is found. The incoming events should go
    /// backwards in time.
    fn process(
        &mut self,
        event: OriginalSyncCallMemberEvent,
    ) -> Option<MilliSecondsSinceUnixEpoch> {
        let CallMemberDiff { joined, left } = event.diff();
        let left = left.into_iter().map(|device_id| {
            let id = CallMemberIdentifier { user_id: event.state_key.clone(), device_id };
            Mark::end(id, event.origin_server_ts)
        });
        let joined = joined.into_iter().map(|device_id| {
            let id = CallMemberIdentifier { user_id: event.state_key.clone(), device_id };
            Mark::start(id, event.origin_server_ts)
        });

        for mark in left.chain(joined) {
            match self.state.take()?.process(mark) {
                FindState::Completed { start, .. } => return Some(start),
                FindState::InProgress(find_start) => {
                    self.state = Some(find_start);
                }
            }
        }

        None
    }
}

/// The source of call member events. This small helper component provides a
/// simple iterator-like interface that extracts call member events from the
/// provided timeline, performing the pagination (`Room::messages()`) if
/// necessary.
struct EventSource {
    events: Vec<OriginalSyncCallMemberEvent>,
    prev_batch: Option<String>,
    room: Room,
}

impl EventSource {
    fn new(timeline: Timeline, room: Room) -> Self {
        let Timeline { prev_batch, events, .. } = timeline;
        let events = events
            .into_iter()
            .filter_map(|event| event.event.deserialize_as::<OriginalSyncCallMemberEvent>().ok())
            .collect();
        Self { events, prev_batch, room }
    }

    async fn next(&mut self) -> Option<OriginalSyncCallMemberEvent> {
        if self.events.is_empty() {
            let options = assign!(MessagesOptions::backward(), {
                from: self.prev_batch.take(),
                filter: assign!(RoomEventFilter::default(), {
                    types: Some(vec![StateEventType::CallMember.to_string()]),
                }),
            });

            let messages = self.room.messages(options).await.ok()?;
            self.prev_batch = messages.end;
            self.events = messages
                .chunk
                .into_iter()
                .filter_map(|event| {
                    event.event.deserialize_as::<OriginalSyncCallMemberEvent>().ok()
                })
                .collect();
        }

        self.events.pop()
    }
}
