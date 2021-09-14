use lazy_static::lazy_static;
use serde_json::{json, Value as JsonValue};

lazy_static! {
    pub static ref MEMBERS: JsonValue = json!({
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
    });
}
