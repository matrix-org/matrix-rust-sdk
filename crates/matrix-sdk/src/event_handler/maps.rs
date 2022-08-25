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

use std::{
    borrow::Borrow,
    collections::{btree_map, BTreeMap},
};

use ruma::{OwnedRoomId, RoomId};

use super::{EventHandlerFn, EventHandlerHandle, EventHandlerWrapper, EventKind};

#[derive(Default)]
pub(super) struct EventHandlerMaps {
    by_kind_type: BTreeMap<KindTypeWrap, Vec<EventHandlerWrapper>>,
    by_kind_type_roomid: BTreeMap<KindTypeRoomIdWrap, Vec<EventHandlerWrapper>>,
}

impl EventHandlerMaps {
    pub fn add(&mut self, handle: EventHandlerHandle, handler_fn: Box<EventHandlerFn>) {
        let wrapper = EventHandlerWrapper { handler_id: handle.handler_id, handler_fn };

        match Key::new(handle) {
            Key::KindType(key) => {
                self.by_kind_type.entry(key).or_default().push(wrapper);
            }
            Key::KindTypeRoomId(key) => {
                self.by_kind_type_roomid.entry(key).or_default().push(wrapper)
            }
        }
    }

    pub fn get_handlers<'a>(
        &'a self,
        ev_kind: EventKind,
        ev_type: &str,
        room_id: Option<&'a RoomId>,
    ) -> impl Iterator<Item = (EventHandlerHandle, &'a EventHandlerFn)> + 'a {
        let non_room_key = KindType { ev_kind, ev_type };
        let maybe_room_key =
            room_id.map(|room_id| KindTypeRoomId { ev_kind, ev_type, room_id: room_id.to_owned() });

        // Use get_key_value instead of just get to be able to access the event_type
        // from the BTreeMap key as &'static str, required for EventHandlerHandle.
        let non_room_kv = self
            .by_kind_type
            .get_key_value(&non_room_key)
            .map(|(key, handlers)| (key.0.ev_type, handlers));
        let maybe_room_kv = maybe_room_key.and_then(|room_key| {
            self.by_kind_type_roomid
                .get_key_value(&room_key)
                .map(|(key, handlers)| (key.0.ev_type, handlers))
        });

        non_room_kv.into_iter().chain(maybe_room_kv).flat_map(move |(ev_type, handlers)| {
            handlers.iter().map(move |wrap| {
                let handle = EventHandlerHandle {
                    ev_kind,
                    ev_type,
                    room_id: room_id.map(ToOwned::to_owned),
                    handler_id: wrap.handler_id,
                };

                (handle, &*wrap.handler_fn)
            })
        })
    }

    pub fn remove(&mut self, handle: EventHandlerHandle) {
        // Generic closure would be a lot nicer
        fn remove_entry<K: Ord>(
            entry: btree_map::Entry<'_, K, Vec<EventHandlerWrapper>>,
            handler_id: u64,
        ) {
            if let btree_map::Entry::Occupied(mut o) = entry {
                let v = o.get_mut();
                v.retain(|e| e.handler_id != handler_id);

                if v.is_empty() {
                    o.remove();
                }
            }
        }

        let handler_id = handle.handler_id;
        match Key::new(handle) {
            Key::KindType(key) => {
                remove_entry(self.by_kind_type.entry(key), handler_id);
            }
            Key::KindTypeRoomId(key) => {
                remove_entry(self.by_kind_type_roomid.entry(key), handler_id);
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_kind_type.len() + self.by_kind_type_roomid.len()
    }
}

enum Key {
    KindType(KindTypeWrap),
    KindTypeRoomId(KindTypeRoomIdWrap),
}

impl Key {
    fn new(handle: EventHandlerHandle) -> Self {
        let EventHandlerHandle { ev_kind, ev_type, room_id, .. } = handle;
        match room_id {
            None => Key::KindType(KindTypeWrap(KindType { ev_kind, ev_type })),
            Some(room_id) => {
                let inner = KindTypeRoomId { ev_kind, ev_type, room_id };
                Key::KindTypeRoomId(KindTypeRoomIdWrap(inner))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KindTypeWrap(KindType<'static>);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KindType<'a> {
    ev_kind: EventKind,
    ev_type: &'a str,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KindTypeRoomIdWrap(KindTypeRoomId<'static>);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KindTypeRoomId<'a> {
    ev_kind: EventKind,
    ev_type: &'a str,
    room_id: OwnedRoomId,
}

// These lifetime-generic impls are what makes it possible to obtain a
// &'static str event type from get_key_value in call_event_handlers.
impl<'a> Borrow<KindType<'a>> for KindTypeWrap {
    fn borrow(&self) -> &KindType<'a> {
        &self.0
    }
}

impl<'a> Borrow<KindTypeRoomId<'a>> for KindTypeRoomIdWrap {
    fn borrow(&self) -> &KindTypeRoomId<'a> {
        &self.0
    }
}
