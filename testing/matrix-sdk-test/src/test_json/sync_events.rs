//! Discrete events found in a sync response.

use once_cell::sync::Lazy;
use serde_json::{Value as JsonValue, json};

// TODO: Move `prev_content` into `unsigned` once ruma supports it
pub static MEMBER: Lazy<JsonValue> = Lazy::new(|| {
    json!({
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
        "prev_content": {
            "avatar_url": null,
            "displayname": "example",
            "membership": "invite"
        },
        "unsigned": {
            "age": 297036,
            "replaces_state": "$151800111315tsynI:localhost"
        }
    })
});

pub static MEMBER_BAN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "avatar_url": null,
            "displayname": "example",
            "membership": "ban"
        },
        "event_id": "$151800140517rfvjc:localhost",
        "origin_server_ts": 151800140,
        "sender": "@example:localhost",
        "state_key": "@banned:localhost",
        "type": "m.room.member",
    })
});

pub static MEMBER_INVITE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "avatar_url": "mxc://localhost/SEsfnsuifSDFSSEF",
            "displayname": "example",
            "membership": "invite",
            "reason": "Looking for support"
        },
        "event_id": "$143273582443PhrSn:localhost",
        "origin_server_ts": 1432735824,
        "room_id": "!jEsUZKDJdhlrceRyVU:localhost",
        "sender": "@example:localhost",
        "state_key": "@invited:localhost",
        "type": "m.room.member",
        "unsigned": {
            "age": 1234,
            "invite_room_state": [
                {
                    "content": {
                        "name": "Example Room"
                    },
                    "sender": "@example:localhost",
                    "state_key": "",
                    "type": "m.room.name"
                },
                {
                    "content": {
                        "join_rule": "invite"
                    },
                    "sender": "@example:localhost",
                    "state_key": "",
                    "type": "m.room.join_rules"
                }
            ]
        }
    })
});

pub static MEMBER_STRIPPED: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "avatar_url": null,
            "displayname": "example",
            "membership": "join"
        },
        "sender": "@example:localhost",
        "state_key": "@example:localhost",
        "type": "m.room.member",
    })
});

pub static NAME: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "name": "room name"
        },
        "event_id": "$15139375513VdeRF:localhost",
        "origin_server_ts": 151393755,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.name",
        "unsigned": {
            "age": 703422
        }
    })
});

pub static NAME_STRIPPED: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "name": "room name"
        },
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.name",
    })
});

pub static POWER_LEVELS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "ban": 50,
            "events": {
                "m.room.avatar": 50,
                "m.room.canonical_alias": 50,
                "m.room.history_visibility": 100,
                "m.room.name": 50,
                "m.room.power_levels": 100,
                "m.room.message": 25
            },
            "events_default": 0,
            "invite": 0,
            "kick": 50,
            "redact": 50,
            "state_default": 50,
            "notifications": {
                "room": 0
            },
            "users": {
                "@example:localhost": 100,
                "@bob:localhost": 0
            },
            "users_default": 0
        },
        "event_id": "$15139375512JaHAW:localhost",
        "origin_server_ts": 151393755,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.power_levels",
        "unsigned": {
            "age": 703422
        }
    })
});

pub static PRESENCE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "avatar_url": "mxc://localhost/wefuiwegh8742w",
            "currently_active": false,
            "last_active_ago": 1,
            "presence": "online",
            "status_msg": "Making cupcakes"
        },
        "sender": "@example:localhost",
        "type": "m.presence"
    })
});

pub static TAG: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "tags": {
                "m.favourite": {
                    "order": 0.0
                },
                "u.work": {
                    "order": 0.9
                }
            }
        },
        "type": "m.tag"
    })
});
