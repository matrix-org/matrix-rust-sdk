//! Example responses from calls to `GET
//! /_matrix/client/v3/rooms/{roomId}/messages`.

use once_cell::sync::Lazy;
use serde_json::{json, Value as JsonValue};

pub static ROOM_MESSAGES_BATCH_1: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "chunk": [
          {
            "age": 1042,
            "content": {
              "body": "hello world",
              "msgtype": "m.text"
            },
            "event_id": "$1444812213350496Caaaf:example.com",
            "origin_server_ts": 1444812213737i64,
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@alice:example.com",
            "type": "m.room.message"
          },
          {
            "age": 20123,
            "content": {
              "body": "the world is big",
              "msgtype": "m.text"
            },
            "event_id": "$1444812213350496Cbbbf:example.com",
            "origin_server_ts": 1444812194656i64,
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@bob:example.com",
            "type": "m.room.message"
          },
          {
            "age": 50789,
            "content": {
              "name": "New room name"
            },
            "event_id": "$1444812213350496Ccccf:example.com",
            "origin_server_ts": 1444812163990i64,
            "unsigned": {
              "prev_content": {
                "name": "Old room name",
              },
            },
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@bob:example.com",
            "state_key": "",
            "type": "m.room.name"
          }
        ],
        "end": "t47409-4357353_219380_26003_2269",
        "start": "t392-516_47314_0_7_1_1_1_11444_1"
    })
});
