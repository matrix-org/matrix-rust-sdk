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
    by_kind: BTreeMap<EventKind, Vec<EventHandlerWrapper>>,
    by_kind_type: BTreeMap<KindTypeWrap, Vec<EventHandlerWrapper>>,
    by_kind_roomid: BTreeMap<KindRoomId, Vec<EventHandlerWrapper>>,
    by_kind_type_roomid: BTreeMap<KindTypeRoomIdWrap, Vec<EventHandlerWrapper>>,
}

impl EventHandlerMaps {
    pub fn add(&mut self, handle: EventHandlerHandle, handler_fn: Box<EventHandlerFn>) {
        let wrapper = EventHandlerWrapper { handler_id: handle.handler_id, handler_fn };

        match Key::new(handle) {
            Key::Kind(key) => {
                self.by_kind.entry(key).or_default().push(wrapper);
            }
            Key::KindType(key) => {
                self.by_kind_type.entry(key).or_default().push(wrapper);
            }
            Key::KindRoomId(key) => {
                self.by_kind_roomid.entry(key).or_default().push(wrapper);
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
        // Use get_key_value instead of just get to be able to access the event_type
        // from the BTreeMap key as &'static str, required for EventHandlerHandle.
        let kind_kv = self.by_kind.get_key_value(&ev_kind).map(|(_, handlers)| (None, handlers));
        let kind_type_kv = self
            .by_kind_type
            .get_key_value(&KindType { ev_kind, ev_type })
            .map(|(key, handlers)| (Some(key.0.ev_type), handlers));
        let maybe_roomid_kvs = room_id
            .map(|r_id| {
                let room_id = r_id.to_owned();
                let kind_roomid_kv = self
                    .by_kind_roomid
                    .get_key_value(&KindRoomId { ev_kind, room_id })
                    .map(|(_, handlers)| (None, handlers));

                let room_id = r_id.to_owned();
                let kind_type_roomid_kv = self
                    .by_kind_type_roomid
                    .get_key_value(&KindTypeRoomId { ev_kind, ev_type, room_id })
                    .map(|(key, handlers)| (Some(key.0.ev_type), handlers));

                [kind_roomid_kv, kind_type_roomid_kv]
            })
            .into_iter()
            .flatten();

        [kind_kv, kind_type_kv].into_iter().chain(maybe_roomid_kvs).flatten().flat_map(
            move |(ev_type, handlers)| {
                handlers.iter().map(move |wrap| {
                    let handle = EventHandlerHandle {
                        ev_kind,
                        ev_type,
                        room_id: room_id.map(ToOwned::to_owned),
                        handler_id: wrap.handler_id,
                    };

                    (handle, &*wrap.handler_fn)
                })
            },
        )
    }

    pub fn remove(&mut self, handle: EventHandlerHandle) {
        // Generic closure would be a lot nicer
        fn remove_entry<K: Ord>(
            map: &mut BTreeMap<K, Vec<EventHandlerWrapper>>,
            key: K,
            handler_id: u64,
        ) {
            if let btree_map::Entry::Occupied(mut o) = map.entry(key) {
                let v = o.get_mut();
                v.retain(|e| e.handler_id != handler_id);

                if v.is_empty() {
                    o.remove();
                }
            }
        }

        let handler_id = handle.handler_id;
        match Key::new(handle) {
            Key::Kind(key) => {
                remove_entry(&mut self.by_kind, key, handler_id);
            }
            Key::KindType(key) => {
                remove_entry(&mut self.by_kind_type, key, handler_id);
            }
            Key::KindRoomId(key) => {
                remove_entry(&mut self.by_kind_roomid, key, handler_id);
            }
            Key::KindTypeRoomId(key) => {
                remove_entry(&mut self.by_kind_type_roomid, key, handler_id);
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_kind_type.len() + self.by_kind_type_roomid.len()
    }
}

enum Key {
    Kind(EventKind),
    KindType(KindTypeWrap),
    KindRoomId(KindRoomId),
    KindTypeRoomId(KindTypeRoomIdWrap),
}

impl Key {
    fn new(handle: EventHandlerHandle) -> Self {
        let EventHandlerHandle { ev_kind, ev_type, room_id, .. } = handle;
        match (ev_type, room_id) {
            (None, None) => Key::Kind(ev_kind),
            (Some(ev_type), None) => Key::KindType(KindTypeWrap(KindType { ev_kind, ev_type })),
            (None, Some(room_id)) => Key::KindRoomId(KindRoomId { ev_kind, room_id }),
            (Some(ev_type), Some(room_id)) => {
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
struct KindRoomId {
    ev_kind: EventKind,
    room_id: OwnedRoomId,
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
