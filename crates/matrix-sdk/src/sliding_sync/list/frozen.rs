use std::collections::BTreeMap;

use imbl::Vector;
use ruma::OwnedRoomId;
use serde::{Deserialize, Serialize};
use tracing::error;

use super::{RoomListEntry, SlidingSyncList};
use crate::sliding_sync::{FrozenSlidingSyncRoom, SlidingSyncRoom};

#[derive(Debug, Serialize, Deserialize)]
pub struct FrozenSlidingSyncList {
    #[serde(default, rename = "rooms_count", skip_serializing_if = "Option::is_none")]
    pub maximum_number_of_rooms: Option<u32>,
    #[serde(default, skip_serializing_if = "Vector::is_empty")]
    pub room_list: Vector<RoomListEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(in super::super) rooms: BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>,
}

impl FrozenSlidingSyncList {
    pub(in super::super) fn freeze(
        source_list: &SlidingSyncList,
        rooms_map: &BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    ) -> Self {
        let mut rooms = BTreeMap::new();
        let mut room_list = Vector::new();

        for room_list_entry in source_list.inner.room_list.read().unwrap().iter() {
            match room_list_entry {
                RoomListEntry::Filled(room_id) | RoomListEntry::Invalidated(room_id) => {
                    if let Some(room) = rooms_map.get(room_id) {
                        rooms.insert(room_id.clone(), room.into());
                    } else {
                        error!(?room_id, "Room exists in the room list entry, but it has not been created; maybe because it was present in the response in a `list.$list.ops` object, but not in the `rooms` object");
                    }
                }

                _ => {}
            };

            room_list.push_back(room_list_entry.freeze_by_ref());
        }

        FrozenSlidingSyncList {
            maximum_number_of_rooms: source_list.maximum_number_of_rooms(),
            room_list,
            rooms,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use imbl::vector;
    use ruma::room_id;
    use serde_json::json;

    use super::FrozenSlidingSyncList;
    use crate::sliding_sync::{list::RoomListEntry, FrozenSlidingSyncRoom};

    #[test]
    fn test_frozen_sliding_sync_list_serialization() {
        assert_eq!(
            serde_json::to_value(&FrozenSlidingSyncList {
                maximum_number_of_rooms: Some(42),
                room_list: vector![RoomListEntry::Empty],
                rooms: {
                    let mut rooms = BTreeMap::new();
                    rooms.insert(
                        room_id!("!foo:bar.org").to_owned(),
                        FrozenSlidingSyncRoom { room_id: room_id!("!foo:bar.org").to_owned() },
                    );

                    rooms
                },
            })
            .unwrap(),
            json!({
                "rooms_count": 42,
                "room_list": ["Empty"],
                "rooms": {
                    "!foo:bar.org": {
                        "room_id": "!foo:bar.org",
                    },
                },
            })
        );
    }
}
