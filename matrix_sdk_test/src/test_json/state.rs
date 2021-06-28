use lazy_static::lazy_static;
use serde_json::{json, Value as JsonValue};

lazy_static! {
    pub static ref ROOM_STATE: JsonValue = json!([
        {
          "content": {
            "join_rule": "public"
          },
          "event_id": "$143273582443PhrSn:example.org",
          "origin_server_ts": 1432735824653i64,
          "room_id": "!SVkFJHzfwvuaIEawgC:localhost",
          "sender": "@example:example.org",
          "state_key": "",
          "type": "m.room.join_rules",
          "unsigned": {
            "age": 1234
          }
        },
        {
          "content": {
            "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
            "displayname": "Alice Margatroid",
            "membership": "join"
          },
          "event_id": "$143273582443PhrSn:example.org",
          "origin_server_ts": 1432735824653i64,
          "room_id": "!SVkFJHzfwvuaIEawgC:localhost",
          "sender": "@example:example.org",
          "state_key": "@alice:example.org",
          "type": "m.room.member",
          "unsigned": {
            "age": 1234
          }
        },
        {
          "content": {
            "creator": "@example:example.org",
            "m.federate": true,
            "predecessor": {
              "event_id": "$something:example.org",
              "room_id": "!oldroom:example.org"
            },
            "room_version": "1"
          },
          "event_id": "$143273582443PhrSn:example.org",
          "origin_server_ts": 1432735824653i64,
          "room_id": "!SVkFJHzfwvuaIEawgC:localhost",
          "sender": "@example:example.org",
          "state_key": "",
          "type": "m.room.create",
          "unsigned": {
            "age": 1234
          }
        },
        {
          "content": {
            "ban": 50,
            "events": {
              "m.room.name": 100,
              "m.room.power_levels": 100
            },
            "events_default": 0,
            "invite": 50,
            "kick": 50,
            "notifications": {
              "room": 20
            },
            "redact": 50,
            "state_default": 50,
            "users": {
              "@example:localhost": 100
            },
            "users_default": 0
          },
          "event_id": "$143273582443PhrSn:example.org",
          "origin_server_ts": 1432735824653i64,
          "room_id": "!SVkFJHzfwvuaIEawgC:localhost",
          "sender": "@example:example.org",
          "state_key": "",
          "type": "m.room.power_levels",
          "unsigned": {
            "age": 1234
          }
        }
    ]);
}
