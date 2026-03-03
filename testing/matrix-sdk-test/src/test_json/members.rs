//! Example responses to `GET /_matrix/client/v3/rooms/{roomId}/members`

use std::sync::LazyLock;

use serde_json::{Value as JsonValue, json};

use super::DEFAULT_TEST_ROOM_ID;

pub static MEMBERS: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "chunk": [
        {
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "room_id": *DEFAULT_TEST_ROOM_ID,
            "sender": "@example:localhost",
            "state_key": "@example:localhost",
            "type": "m.room.member",
            "unsigned": {
                "age": 2970366,
            }
        }
        ]
    })
});
