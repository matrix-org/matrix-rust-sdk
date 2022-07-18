//! Example responses to `GET /_matrix/client/v3/rooms/{roomId}/members`

use once_cell::sync::Lazy;
use serde_json::{json, Value as JsonValue};

pub static MEMBERS: Lazy<JsonValue> = Lazy::new(|| {
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
