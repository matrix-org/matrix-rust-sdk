use std::collections::BTreeMap;

use imbl::Vector;
use ruma::OwnedRoomId;
use serde::{Deserialize, Serialize};

use super::{
    super::{FrozenSlidingSyncRoom, SlidingSyncRoom},
    RoomListEntry, SlidingSyncList,
};

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
                    rooms.insert(
                        room_id.clone(),
                        rooms_map.get(room_id).expect("room doesn't exist").into(),
                    );
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
    use imbl::vector;
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use ruma::{
        api::client::sync::sync_events::v4, events::room::message::RoomMessageEventContent,
        room_id, serde::Raw,
    };
    use serde_json::json;

    use super::*;

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
                        FrozenSlidingSyncRoom {
                            room_id: room_id!("!foo:bar.org").to_owned(),
                            inner: v4::SlidingSyncRoom::default(),
                            timeline_queue: vector![TimelineEvent::new(
                                Raw::new(&json!({
                                    "content": RoomMessageEventContent::text_plain("let it gooo!"),
                                    "type": "m.room.message",
                                    "event_id": "$xxxxx:example.org",
                                    "room_id": "!someroom:example.com",
                                    "origin_server_ts": 2189,
                                    "sender": "@bob:example.com",
                                }))
                                .unwrap()
                                .cast(),
                            )
                            .into()],
                        },
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
                        "inner": {},
                        "timeline": [
                            {
                                "event": {
                                    "content": {
                                        "body": "let it gooo!",
                                        "msgtype": "m.text",
                                    },
                                    "event_id": "$xxxxx:example.org",
                                    "origin_server_ts": 2189,
                                    "room_id": "!someroom:example.com",
                                    "sender": "@bob:example.com",
                                    "type": "m.room.message",
                                },
                                "encryption_info": null,
                            }
                        ],
                    },
                },
            })
        );
    }
}
