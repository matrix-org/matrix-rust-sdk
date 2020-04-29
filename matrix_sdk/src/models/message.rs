use std::collections::{vec_deque::IntoIter, VecDeque};

use crate::events::collections::all::RoomEvent;
use crate::events::room::message::MessageEvent;
use crate::events::EventJson;

use serde::{de, ser};

pub(crate) mod ser_deser {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<MessageQueue, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let messages: VecDeque<EventJson<RoomEvent>> = de::Deserialize::deserialize(deserializer)?;

        // TODO this should probably bail out if deserialization fails not skip
        let msgs: VecDeque<RoomEvent> = messages
            .into_iter()
            .flat_map(|json| json.deserialize())
            .collect();

        Ok(MessageQueue { msgs })
    }

    pub fn serialize<S>(msgs: &MessageQueue, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        use ser::Serialize;

        msgs.msgs.serialize(serializer)
    }
}

/// A queue that holds at most 10 messages received from the server.
#[derive(Clone, Debug, Default)]
pub struct MessageQueue {
    msgs: VecDeque<RoomEvent>,
}

impl PartialEq for MessageQueue {
    fn eq(&self, other: &MessageQueue) -> bool {
        self.msgs.len() == other.msgs.len()
            && self
                .msgs
                .iter()
                .zip(other.msgs.iter())
                .all(|(a, b)| match (a, b) {
                    (RoomEvent::RoomMessage(msg_a), RoomEvent::RoomMessage(msg_b)) => {
                        msg_a.event_id == msg_b.event_id
                    }
                    _ => false,
                })
    }
}

impl MessageQueue {
    /// Create a new empty `MessageQueue`.
    pub fn new() -> Self {
        Self {
            msgs: VecDeque::with_capacity(20),
        }
    }

    /// Appends a `MessageEvent` to the end of the `MessageQueue`.
    ///
    /// Removes the oldest element in the queue if there are more than 10 elements.
    pub fn push(&mut self, msg: MessageEvent) -> bool {
        self.msgs.push_back(RoomEvent::RoomMessage(msg));
        if self.msgs.len() > 10 {
            self.msgs.pop_front();
        }
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = &RoomEvent> {
        self.msgs.iter()
    }
}

impl IntoIterator for MessageQueue {
    type Item = RoomEvent;
    type IntoIter = IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.msgs.into_iter()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashMap;
    use std::convert::TryFrom;

    use crate::events::{collections::all::RoomEvent, EventJson};
    use crate::identifiers::{RoomId, UserId};
    use crate::{state::ClientState, Room};

    #[test]
    fn serialize() {
        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let mut room = Room::new(&id, &user);

        let json = std::fs::read_to_string("../test_data/events/message_text.json").unwrap();
        let event = serde_json::from_str::<EventJson<RoomEvent>>(&json).unwrap();

        let mut msgs = MessageQueue::new();
        if let Ok(ev) = event.deserialize() {
            if let RoomEvent::RoomMessage(msg) = ev {
                msgs.push(msg);
            }
        }
        room.messages = msgs;

        let mut joined_rooms = HashMap::new();
        joined_rooms.insert(id, room);

        assert_eq!(
            r#"{
  "!roomid:example.com": {
    "room_id": "!roomid:example.com",
    "room_name": {
      "name": null,
      "canonical_alias": null,
      "aliases": [],
      "heroes": [],
      "joined_member_count": null,
      "invited_member_count": null
    },
    "own_user_id": "@example:example.com",
    "creator": null,
    "members": {},
    "messages": [
      {
        "type": "m.room.message",
        "content": {
          "body": "is dancing",
          "format": "org.matrix.custom.html",
          "formatted_body": "<strong>is dancing</strong>",
          "msgtype": "m.text"
        },
        "event_id": "$152037280074GZeOm:localhost",
        "origin_server_ts": 1520372800469,
        "sender": "@example:localhost",
        "unsigned": {
          "age": 598971425
        }
      }
    ],
    "typing_users": [],
    "power_levels": null,
    "encrypted": false,
    "unread_highlight": null,
    "unread_notifications": null,
    "tombstone": null
  }
}"#,
            serde_json::to_string_pretty(&joined_rooms).unwrap()
        );
    }

    #[test]
    fn deserialize() {
        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let mut room = Room::new(&id, &user);

        let json = std::fs::read_to_string("../test_data/events/message_text.json").unwrap();
        let event = serde_json::from_str::<EventJson<RoomEvent>>(&json).unwrap();

        let mut msgs = MessageQueue::new();
        if let Ok(ev) = event.deserialize() {
            if let RoomEvent::RoomMessage(msg) = ev {
                msgs.push(msg);
            }
        }
        room.messages = msgs;

        let mut joined_rooms = HashMap::new();
        joined_rooms.insert(id, room.clone());

        let json = r#"{
  "!roomid:example.com": {
    "room_id": "!roomid:example.com",
    "room_name": {
      "name": null,
      "canonical_alias": null,
      "aliases": [],
      "heroes": [],
      "joined_member_count": null,
      "invited_member_count": null
    },
    "own_user_id": "@example:example.com",
    "creator": null,
    "members": {},
    "messages": [
      {
        "type": "m.room.message",
        "content": {
          "body": "is dancing",
          "format": "org.matrix.custom.html",
          "formatted_body": "<strong>is dancing</strong>",
          "msgtype": "m.text"
        },
        "event_id": "$152037280074GZeOm:localhost",
        "origin_server_ts": 1520372800469,
        "sender": "@example:localhost",
        "unsigned": {
          "age": 598971425
        }
      }
    ],
    "typing_users": [],
    "power_levels": null,
    "encrypted": false,
    "unread_highlight": null,
    "unread_notifications": null,
    "tombstone": null
  }
}"#;
        assert_eq!(
            joined_rooms,
            serde_json::from_str::<HashMap<RoomId, Room>>(json).unwrap()
        );
    }
}
