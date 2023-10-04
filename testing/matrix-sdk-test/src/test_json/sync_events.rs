//! Discrete events found in a sync response.

use once_cell::sync::Lazy;
use serde_json::{json, Value as JsonValue};

pub static ALIAS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "alias": "#tutorial:localhost"
        },
        "event_id": "$15139375513VdeRF:localhost",
        "origin_server_ts": 151393755,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.canonical_alias",
        "unsigned": {
            "age": 703422
        }
    })
});

pub static ALIASES: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "aliases": [
                "#tutorial:localhost"
            ]
        },
        "event_id": "$15139375516NUgtD:localhost",
        "origin_server_ts": 151393755,
        "sender": "@example:localhost",
        "state_key": "localhost",
        "type": "m.room.aliases",
        "unsigned": {
            "age": 703422
        }
    })
});

pub static CREATE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "creator": "@example:localhost",
            "m.federate": true,
            "room_version": "1"
        },
        "event_id": "$151957878228ekrDs:localhost",
        "origin_server_ts": 15195787,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.create",
        "unsigned": {
          "age": 139298
        }
    })
});

pub static DIRECT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "@invited:localhost": ["!SVkFJHzfwvuaIEawgC:localhost"],
        },
        "event_id": "$757957878228ekrDs:localhost",
        "origin_server_ts": 17195787,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.direct",
        "unsigned": {
          "age": 139298
        }
    })
});

pub static FULLY_READ: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "event_id": "$someplace:example.org"
        },
        "room_id": "!somewhere:example.org",
        "type": "m.fully_read"
    })
});

pub static HISTORY_VISIBILITY: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "history_visibility": "world_readable"
        },
        "event_id": "$151957878235ricnD:localhost",
        "origin_server_ts": 151957878,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.history_visibility",
        "unsigned": {
          "age": 1392989
        }
    })
});

pub static JOIN_RULES: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "join_rule": "public"
        },
        "event_id": "$151957878231iejdB:localhost",
        "origin_server_ts": 151957878,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.room.join_rules",
        "unsigned": {
          "age": 1392989
        }
    })
});

pub static ENCRYPTION_CONTENT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "algorithm": "m.megolm.v1.aes-sha2",
        "rotation_period_ms": 604800000,
        "rotation_period_msgs": 100
    })
});

pub static ENCRYPTION: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": *ENCRYPTION_CONTENT,
        "event_id": "$143273582443PhrSn:example.org",
        "origin_server_ts": 1432735824653u64,
        "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
        "sender": "@example:example.org",
        "state_key": "",
        "type": "m.room.encryption",
        "unsigned": {
            "age": 1234
        }
    })
});

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

// Make @invited:localhost a member (note the confusing name)
pub static MEMBER_ADDITIONAL: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "membership": "join",
        },
        "event_id": "$747273582443PhrSn:localhost",
        "origin_server_ts": 1472735824,
        "sender": "@example:localhost",
        "state_key": "@invited:localhost",
        "type": "m.room.member",
        "unsigned": {
            "age": 1234
        }
    })
});

// Make @invited:localhost leave the room (note the confusing name)
pub static MEMBER_LEAVE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "membership": "leave",
        },
        "event_id": "$747273582443PhrS9:localhost",
        "origin_server_ts": 1472735820,
        "sender": "@example:localhost",
        "state_key": "@invited:localhost",
        "type": "m.room.member",
        "unsigned": {
            "age": 1234
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

// TODO: Move `prev_content` into `unsigned` once ruma supports it
pub static MEMBER_NAME_CHANGE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "avatar_url": null,
            "displayname": "changed",
            "membership": "join"
        },
        "event_id": "$151800234427abgho:localhost",
        "membership": "join",
        "origin_server_ts": 151800152,
        "sender": "@example:localhost",
        "state_key": "@example:localhost",
        "type": "m.room.member",
        "prev_content": {
            "avatar_url": null,
            "displayname": "example",
            "membership": "join"
        },
        "unsigned": {
            "age": 297032,
            "replaces_state": "$151800140517rfvjc:localhost"
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

pub static PUSH_RULES: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "global": {
                "content": [
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "sound",
                                "value": "default"
                            },
                            {
                                "set_tweak": "highlight"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "pattern": "example",
                        "rule_id": ".m.rule.contains_user_name"
                    }
                ],
                "override": [
                    {
                        "actions": [
                            "dont_notify"
                        ],
                        "conditions": [],
                        "default": true,
                        "enabled": false,
                        "rule_id": ".m.rule.master"
                    },
                    {
                        "actions": [
                            "dont_notify"
                        ],
                        "conditions": [
                            {
                                "key": "content.msgtype",
                                "kind": "event_match",
                                "pattern": "m.notice"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.suppress_notices"
                    }
                ],
                "room": [
                    {
                      "actions": [
                        "notify",
                        {
                          "set_tweak": "sound",
                          "value": "default"
                        }
                      ],
                      "rule_id": "!SVkFJHzfwvuaIEawgC:localhost",
                      "default": false,
                      "enabled": true
                    }
                ],
                "sender": [],
                "underride": [
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "sound",
                                "value": "ring"
                            },
                            {
                                "set_tweak": "highlight",
                                "value": false
                            }
                        ],
                        "conditions": [
                            {
                                "key": "type",
                                "kind": "event_match",
                                "pattern": "m.call.invite"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.call"
                    },
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "sound",
                                "value": "default"
                            },
                            {
                                "set_tweak": "highlight"
                            }
                        ],
                        "conditions": [
                            {
                                "kind": "contains_display_name"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.contains_display_name"
                    },
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "sound",
                                "value": "default"
                            },
                            {
                                "set_tweak": "highlight",
                                "value": false
                            }
                        ],
                        "conditions": [
                            {
                                "is": "2",
                                "kind": "room_member_count"
                            },
                            {
                                "key": "type",
                                "kind": "event_match",
                                "pattern": "m.room.message"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.room_one_to_one"
                    },
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "sound",
                                "value": "default"
                            },
                            {
                                "set_tweak": "highlight",
                                "value": false
                            }
                        ],
                        "conditions": [
                            {
                                "key": "type",
                                "kind": "event_match",
                                "pattern": "m.room.member"
                            },
                            {
                                "key": "content.membership",
                                "kind": "event_match",
                                "pattern": "invite"
                            },
                            {
                                "key": "state_key",
                                "kind": "event_match",
                                "pattern": "@example:localhost"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.invite_for_me"
                    },
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "highlight",
                                "value": false
                            }
                        ],
                        "conditions": [
                            {
                                "key": "type",
                                "kind": "event_match",
                                "pattern": "m.room.member"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.member_event"
                    },
                    {
                        "actions": [
                            "notify",
                            {
                                "set_tweak": "highlight",
                                "value": false
                            }
                        ],
                        "conditions": [
                            {
                                "key": "type",
                                "kind": "event_match",
                                "pattern": "m.room.message"
                            }
                        ],
                        "default": true,
                        "enabled": true,
                        "rule_id": ".m.rule.message"
                    }
                ]
            }
        },
        "type": "m.push_rules"
    })
});

pub static READ_RECEIPT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "$example": {
                "m.read": {
                    "@example:localhost": {
                        "ts": 1436451550
                    }
                }
            }
        },
        "room_id": "!test:localhost",
        "type": "m.receipt"
    })
});

pub static READ_RECEIPT_OTHER: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "$other": {
                "m.read": {
                    "@example:localhost": {
                        "ts": 1436964550
                    }
                }
            }
        },
        "room_id": "!test:localhost",
        "type": "m.receipt"
    })
});

pub static REDACTED_INVALID: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {},
        "event_id": "$15275046980maRLj:localhost",
        "origin_server_ts": 1527504698,
        "sender": "@example:localhost",
        "type": "m.room.message"
    })
});

pub static REDACTED_STATE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {},
        "event_id": "$example_id:example.org",
        "origin_server_ts": 153232493,
        "sender": "@example:example.org",
        "state_key": "test_state_key",
        "type": "m.some.state",
        "unsigned": {
            "age": 3069315,
            "redacted_because": {
                "content": {},
                "event_id": "$redaction_example_id:example.org",
                "origin_server_ts": 153232494,
                "redacts": "$example_id:example.org",
                "sender": "@example:example:org",
                "type": "m.room.redaction",
                "unsigned": {"age": 30693147}
            },
            "redacted_by": "$redaction_example_id:example.org"
        }
    })
});

pub static ROOM_AVATAR: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "info": {
                "h": 398,
                "mimetype": "image/jpeg",
                "size": 31037,
                "w": 394
            },
            "url": "mxc://domain.com/JWEIFJgwEIhweiWJE"
        },
        "event_id": "$143273582443PhrSn:domain.com",
        "origin_server_ts": 143273582,
        "room_id": "!jEsUZKDJdhlrceRyVU:domain.com",
        "sender": "@example:domain.com",
        "state_key": "",
        "type": "m.room.avatar",
        "unsigned": {
            "age": 1234
        }
    })
});

pub static TAG: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "tags": {
                "u.work": {
                    "order": 0.9
                }
            }
        },
        "type": "m.tag"
    })
});

// TODO: Move `prev_content` into `unsigned` once ruma supports it
pub static TOPIC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "topic": "😀"
        },
        "event_id": "$151957878228ssqrJ:localhost",
        "origin_server_ts": 151957878,
        "sender": "@example:localhost",
        "state_key": "",
        "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
        "type": "m.room.topic",
        "prev_content": {
            "topic": "test"
        },
        "unsigned": {
          "age": 1392989,
          "prev_sender": "@example:localhost",
          "replaces_state": "$151957069225EVYKm:localhost"
        }
    })
});

pub static TYPING: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "user_ids": [
                "@alice:matrix.org",
                "@bob:example.com"
            ]
        },
        "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
        "type": "m.typing"
    })
});

pub static TOPIC_REDACTION: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {},
        "redacts": "$151957878228ssqrJ:localhost",
        "event_id": "$151957878228ssqrJ_REDACTION:localhost",
        "origin_server_ts": 151957879,
        "sender": "@example:localhost",
        "type": "m.room.redaction",
        "unsigned": {
          "age": 1392990,
          "prev_sender": "@example:localhost",
        }
    })
});
