//! Complete sync responses.

use once_cell::sync::Lazy;
use ruma::{RoomId, room_id};
use serde_json::{Value as JsonValue, json};

use crate::DEFAULT_TEST_ROOM_ID;

pub static SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "account_data": {
            "events": [
                {
                    "content": {
                        "ignored_users": {
                            "@someone:example.org": {}
                        }
                    },
                    "type": "m.ignored_user_list"
                }
            ]
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": [
                            {
                                "content": {
                                    "event_id": "$someplace:example.org"
                                },
                                "room_id": "!roomid:room.com",
                                "type": "m.fully_read"
                            }
                        ]
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "room_id": *DEFAULT_TEST_ROOM_ID,
                                "type": "m.receipt"
                            },
                        ]
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 2970366,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "history_visibility": "shared"
                                },
                                "event_id": "$15139375515VaJEY:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.history_visibility",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "aliases": [
                                        "#tutorial:localhost"
                                    ]
                                },
                                "event_id": "$15139375516NUgtD:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "localhost",
                                "type": "m.room.aliases",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "topic": "room topic"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.topic",
                                "unsigned": {
                                    "age": 1392989709,
                                    "prev_content": {
                                        "topic": "test"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$151957069225EVYKm:localhost"
                                }
                            },
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "alias": "#tutorial:localhost"
                                },
                                "event_id": "$15139375513VdeRF:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.canonical_alias",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example2",
                                    "membership": "join"
                                },
                                "event_id": "$152034824468gOeNB:localhost",
                                "membership": "join",
                                "origin_server_ts": 152034824000000_u64,
                                "sender": "@example2:localhost",
                                "state_key": "@example2:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 623527289,
                                    "prev_content": {
                                        "membership": "leave"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$152034819067QWJxM:localhost"
                                }
                            },
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": [
                {
                    "content": {
                        "avatar_url": "mxc://localhost/wefuiwegh8742w",
                        "currently_active": false,
                        "last_active_ago": 1,
                        "presence": "online",
                        "status_msg": "Making cupcakes"
                    },
                    "sender": "@example:localhost",
                    "type": "m.presence"
                }
            ]
        }
    })
});

pub static DEFAULT_SYNC_SUMMARY: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {
                        "m.heroes": [
                          "@example2:localhost"
                        ],
                        "m.joined_member_count": 2,
                        "m.invited_member_count": 0
                      },
                    "account_data": {
                        "events": [
                            {
                                "content": {
                                    "ignored_users": {
                                        "@someone:example.org": {}
                                    }
                                },
                                "type": "m.ignored_user_list"
                            }
                        ]
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "type": "m.receipt"
                            },
                            {
                                "content": {
                                    "event_id": "$someplace:example.org"
                                },
                                "room_id": "!roomid:room.com",
                                "type": "m.fully_read"
                            }
                        ]
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 297036,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "history_visibility": "shared"
                                },
                                "event_id": "$15139375515VaJEY:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.history_visibility",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "topic": "room topic"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.topic",
                                "unsigned": {
                                    "age": 1392989709,
                                    "prev_content": {
                                        "topic": "test"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$151957069225EVYKm:localhost"
                                }
                            },
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example2",
                                    "membership": "join"
                                },
                                "event_id": "$152034824468gOeNB:localhost",
                                "membership": "join",
                                "origin_server_ts": 152034824000000_u64,
                                "sender": "@example2:localhost",
                                "state_key": "@example2:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 623527289,
                                    "prev_content": {
                                        "membership": "leave"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$152034819067QWJxM:localhost"
                                }
                            },
                            {
                                "content": {
                                  "membership": "leave",
                                  "reason": "offline",
                                  "avatar_url": "avatar.com",
                                  "displayname": "example"
                                },
                                "event_id": "$1585345508297748AIUBh:matrix.org",
                                "origin_server_ts": 158534550000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                  "replaces_state": "$1585345354296486IGZfp:localhost",
                                  "prev_content": {
                                    "avatar_url": "avatar.com",
                                    "displayname": "example",
                                    "membership": "join"
                                  },
                                  "prev_sender": "@example2:localhost",
                                  "age": 6992
                                },
                                "room_id": "!roomid:room.com"
                              }
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": [
                {
                    "content": {
                        "avatar_url": "mxc://localhost/wefuiwegh8742w",
                        "currently_active": false,
                        "last_active_ago": 1,
                        "presence": "online",
                        "status_msg": "Making cupcakes"
                    },
                    "sender": "@example:localhost",
                    "type": "m.presence"
                }
            ]
        }
    })
});

pub static MORE_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_2",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": []
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "type": "m.receipt"
                            },
                            {
                                "content": {
                                    "user_ids": [
                                        "@alice:matrix.org",
                                        "@bob:example.com"
                                    ]
                                },
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "type": "m.typing"
                            }
                        ]
                    },
                    "state": {
                        "events": []
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            },
                            {
                                "content": {
                                    "body": " * edited message",
                                    "m.new_content": {
                                        "body": "edited message",
                                        "msgtype": "m.text"
                                    },
                                    "m.relates_to": {
                                        "event_id": "$someeventid:localhost",
                                        "rel_type": "m.replace"
                                    },
                                    "msgtype": "m.text"
                                },
                                "event_id": "$editevid:localhost",
                                "origin_server_ts": 159026265000000_u64,
                                "sender": "@alice:matrix.org",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {
                                    "reason": "😀"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.redaction",
                                "redacts": "$151957878228ssqrj:localhost",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {},
                                "event_id": "$15275046980maRLj:localhost",
                                "origin_server_ts": 152750469000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 19334,
                                    "redacted_because": {
                                        "content": {},
                                        "event_id": "$15275047031IXQRi:localhost",
                                        "origin_server_ts": 152750470000000_u64,
                                        "redacts": "$15275046980maRLj:localhost",
                                        "sender": "@example:localhost",
                                        "type": "m.room.redaction",
                                        "unsigned": {
                                            "age": 14523
                                        }
                                    },
                                    "redacted_by": "$15275047031IXQRi:localhost"
                                }
                            },
                            {
                                "content": {
                                    "m.relates_to": {
                                        "event_id": "$15275047031IXQRi:localhost",
                                        "key": "👍",
                                        "rel_type": "m.annotation"
                                    }
                                },
                                "event_id": "$15275047031IXQRi:localhost",
                                "origin_server_ts": 159027581000000_u64,
                                "sender": "@alice:matrix.org",
                                "type": "m.reaction",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {
                                    "body": "This is a notice",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<em>This is a notice</em>",
                                    "msgtype": "m.notice"
                                },
                                "event_id": "$098237280074GZeOm:localhost",
                                "origin_server_ts": 162037280000000_u64,
                                "sender": "@bot:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 25
                                }
                            },
                        ],
                        "limited": false,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": []
        }
    })
});

pub static MORE_SYNC_2: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "next_batch": "s526_47314_0_7_1_1_1_11444_3",
        "rooms": {
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm2:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            },
                            {
                                "content": {
                                    "body": " * edited message",
                                    "m.new_content": {
                                        "body": "edited message",
                                        "msgtype": "m.text"
                                    },
                                    "m.relates_to": {
                                        "event_id": "$someeventid:localhost",
                                        "rel_type": "m.replace"
                                    },
                                    "msgtype": "m.text"
                                },
                                "event_id": "$editevid2:localhost",
                                "origin_server_ts": 159026265000000_u64,
                                "sender": "@alice:matrix.org",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {
                                    "reason": "😀"
                                },
                                "event_id": "$151957878228ssqrJ2:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.redaction",
                                "redacts": "$151957878228ssqrj:localhost",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {},
                                "event_id": "$15275046980maRLj2:localhost",
                                "origin_server_ts": 152750469000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 19334,
                                    "redacted_because": {
                                        "content": {},
                                        "event_id": "$15275047031IXQRi:localhost",
                                        "origin_server_ts": 152750470000000_u64,
                                        "redacts": "$15275046980maRLj:localhost",
                                        "sender": "@example:localhost",
                                        "type": "m.room.redaction",
                                        "unsigned": {
                                            "age": 14523
                                        }
                                    },
                                    "redacted_by": "$15275047031IXQRi:localhost"
                                }
                            },
                            {
                                "content": {
                                    "m.relates_to": {
                                        "event_id": "$15275047031IXQRi:localhost",
                                        "key": "👍",
                                        "rel_type": "m.annotation"
                                    }
                                },
                                "event_id": "$15275047031IXQRi2:localhost",
                                "origin_server_ts": 159027581000000_u64,
                                "sender": "@alice:matrix.org",
                                "type": "m.reaction",
                                "unsigned": {
                                    "age": 85
                                }
                            },
                            {
                                "content": {
                                    "body": "This is a notice",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<em>This is a notice</em>",
                                    "msgtype": "m.notice"
                                },
                                "event_id": "$098237280074GZeOm2:localhost",
                                "origin_server_ts": 162037280000000_u64,
                                "sender": "@bot:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 25
                                }
                            },
                        ],
                        "limited": false,
                        "prev_batch": "s526_47314_0_7_1_1_1_11444_2"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
        },
    })
});

pub static INVITE_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_2",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {
                "!696r7674:example.com": {
                  "invite_state": {
                    "events": [
                      {
                        "sender": "@alice:example.com",
                        "type": "m.room.name",
                        "state_key": "",
                        "content": {
                          "name": "My Room Name"
                        }
                      },
                      {
                        "sender": "@alice:example.com",
                        "type": "m.room.member",
                        "state_key": "@bob:example.com",
                        "content": {
                          "membership": "invite"
                        }
                      }
                    ]
                  }
                }
              },
            "join": {},
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": [
                {
                    "content": {
                        "avatar_url": "mxc://localhost/wefuiwegh8742w",
                        "currently_active": false,
                        "last_active_ago": 1,
                        "presence": "online",
                        "status_msg": "Making cupcakes"
                    },
                    "sender": "@example:localhost",
                    "type": "m.presence"
                }
            ]
        }
    })
});

pub static LEAVE_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "account_data": {
            "events": [
                {
                    "content": {
                        "ignored_users": {
                            "@someone:example.org": {}
                        }
                    },
                    "type": "m.ignored_user_list"
                }
            ]
        },
        "rooms": {
            "invite": {},
            "join": {},
            "leave": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": []
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "type": "m.receipt"
                            },
                            {
                                "content": {
                                    "event_id": "$someplace:example.org"
                                },
                                "room_id": "!roomid:room.com",
                                "type": "m.fully_read"
                            }
                        ]
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 297036,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "history_visibility": "shared"
                                },
                                "event_id": "$15139375515VaJEY:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.history_visibility",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "aliases": [
                                        "#tutorial:localhost"
                                    ]
                                },
                                "event_id": "$15139375516NUgtD:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "localhost",
                                "type": "m.room.aliases",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "topic": "room topic"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.topic",
                                "unsigned": {
                                    "age": 1392989709,
                                    "prev_content": {
                                        "topic": "test"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$151957069225EVYKm:localhost"
                                }
                            },
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "alias": "#tutorial:localhost"
                                },
                                "event_id": "$15139375513VdeRF:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.canonical_alias",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example2",
                                    "membership": "join"
                                },
                                "event_id": "$152034824468gOeNB:localhost",
                                "membership": "join",
                                "origin_server_ts": 152034824000000_u64,
                                "sender": "@example2:localhost",
                                "state_key": "@example2:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 623527289,
                                    "prev_content": {
                                        "membership": "leave"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$152034819067QWJxM:localhost"
                                }
                            },
                            {
                                "content": {
                                  "membership": "leave",
                                  "reason": "offline",
                                  "avatar_url": "mxc://avatar.com/ursn982srs2S",
                                  "displayname": "example"
                                },
                                "event_id": "$1585345508297748AIUBh:matrix.org",
                                "origin_server_ts": 158534550000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                  "replaces_state": "$1585345354296486IGZfp:localhost",
                                  "prev_content": {
                                    "avatar_url": "mxc://avatar.com/ursn982srs2S",
                                    "displayname": "example",
                                    "membership": "join"
                                  },
                                  "prev_sender": "@example2:localhost",
                                  "age": 6992
                                },
                                "room_id": "!roomid:room.com"
                              }
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            }
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": [
                {
                    "content": {
                        "avatar_url": "mxc://localhost/wefuiwegh8742w",
                        "currently_active": false,
                        "last_active_ago": 1,
                        "presence": "online",
                        "status_msg": "Making cupcakes"
                    },
                    "sender": "@example:localhost",
                    "type": "m.presence"
                }
            ]
        }
    })
});

pub static LEAVE_SYNC_EVENT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "account_data": {
            "events": []
        },
        "to_device": {
            "events": []
        },
        "device_lists": {
            "changed": [],
            "left": []
        },
        "presence": {
            "events": []
        },
        "rooms": {
            "join": {},
            "invite": {},
            "leave": {
                *DEFAULT_TEST_ROOM_ID: {
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "membership": "leave"
                                },
                                "origin_server_ts": 158957809000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "replaces_state": "$blahblah",
                                    "prev_content": {
                                        "avatar_url": null,
                                        "displayname": "me",
                                        "membership": "invite"
                                    },
                                    "prev_sender": "@2example:localhost",
                                    "age": 1757
                                },
                                "event_id": "$lQQ116Y-XqcjpSUGpuz36rNntUvOSpTjuaIvmtQ2AwA"
                            }
                        ],
                        "prev_batch": "tokenTOKEN",
                        "limited": false
                    },
                    "state": {
                        "events": []
                    },
                    "account_data": {
                        "events": []
                    }
                }
            }
        },
        "groups": {
            "join": {},
            "invite": {},
            "leave": {}
        },
        "device_one_time_keys_count": {
            "signed_curve25519": 50
        },
        "next_batch": "s1380317562_757269739_1655566_503953763_334052043_1209862_55290918_65705002_101146"
    })
});

pub static JOIN_SPACE_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "account_data": {
            "events": [
                {
                    "content": {
                        "ignored_users": {
                            "@someone:example.org": {}
                        }
                    },
                    "type": "m.ignored_user_list"
                }
            ]
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": [
                            {
                                "content": {
                                    "event_id": "$someplace:example.org"
                                },
                                "room_id": "!roomid:room.com",
                                "type": "m.fully_read"
                            }
                        ]
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "room_id": *DEFAULT_TEST_ROOM_ID,
                                "type": "m.receipt"
                            },
                        ]
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 2970366,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "history_visibility": "shared"
                                },
                                "event_id": "$15139375515VaJEY:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.history_visibility",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost",
                                    "type": "m.space"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "aliases": [
                                        "#tutorial:localhost"
                                    ]
                                },
                                "event_id": "$15139375516NUgtD:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "localhost",
                                "type": "m.room.aliases",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "topic": "room topic"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.topic",
                                "unsigned": {
                                    "age": 1392989709,
                                    "prev_content": {
                                        "topic": "test"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$151957069225EVYKm:localhost"
                                }
                            },
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "alias": "#tutorial:localhost"
                                },
                                "event_id": "$15139375513VdeRF:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.canonical_alias",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "example2",
                                    "membership": "join"
                                },
                                "event_id": "$152034824468gOeNB:localhost",
                                "membership": "join",
                                "origin_server_ts": 152034824000000_u64,
                                "sender": "@example2:localhost",
                                "state_key": "@example2:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 623527289,
                                    "prev_content": {
                                        "membership": "leave"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$152034819067QWJxM:localhost"
                                }
                            },
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": [
                {
                    "content": {
                        "avatar_url": "mxc://localhost/wefuiwegh8742w",
                        "currently_active": false,
                        "last_active_ago": 1,
                        "presence": "online",
                        "status_msg": "Making cupcakes"
                    },
                    "sender": "@example:localhost",
                    "type": "m.presence"
                }
            ]
        }
    })
});

/// In the [`MIXED_SYNC`], the room id of the joined room.
pub static MIXED_JOINED_ROOM_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!SVkFJHzfwvuaIEawgC:localhost"));
/// In the [`MIXED_SYNC`], the room id of the left room.
pub static MIXED_LEFT_ROOM_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!SVkFJHzfwvuaIEawgD:localhost"));
/// In the [`MIXED_SYNC`], the room id of the invited room.
pub static MIXED_INVITED_ROOM_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!SVkFJHzfwvuaIEawgE:localhost"));
/// In the [`MIXED_SYNC`], the room id of the knocked room.
pub static MIXED_KNOCKED_ROOM_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!SVkFJHzfwvuaIEawgF:localhost"));

/// A sync that contains updates to joined/invited/knocked/left rooms.
pub static MIXED_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "account_data": {
            "events": []
        },
        "to_device": {
            "events": []
        },
        "device_lists": {
            "changed": [],
            "left": []
        },
        "presence": {
            "events": []
        },
        "rooms": {
            "join": {
                *MIXED_JOINED_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": [
                            {
                                "content": {
                                    "event_id": "$someplace:example.org"
                                },
                                "room_id": "!roomid:room.com",
                                "type": "m.fully_read"
                            }
                        ]
                    },
                    "ephemeral": {
                        "events": [
                            {
                                "content": {
                                    "$151680659217152dPKjd:localhost": {
                                        "m.read": {
                                            "@example:localhost": {
                                                "ts": 151680989
                                            }
                                        }
                                    }
                                },
                                "room_id": *MIXED_JOINED_ROOM_ID,
                                "type": "m.receipt"
                            },
                        ]
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "alias": "#tutorial:localhost"
                                },
                                "event_id": "$15139375513VdeRF:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.canonical_alias",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "invite": {
                *MIXED_INVITED_ROOM_ID: {
                  "invite_state": {
                    "events": [
                      {
                        "sender": "@alice:example.com",
                        "type": "m.room.name",
                        "state_key": "",
                        "content": {
                          "name": "My Room Name"
                        }
                      },
                      {
                        "sender": "@alice:example.com",
                        "type": "m.room.member",
                        "state_key": "@bob:example.com",
                        "content": {
                          "membership": "invite"
                        }
                      }
                    ]
                  }
                }
            },
            "knock": {
                *MIXED_KNOCKED_ROOM_ID: {
                  "knock_state": {
                    "events": [
                      {
                        "sender": "@alice:example.com",
                        "type": "m.room.name",
                        "state_key": "",
                        "content": {
                          "name": "My Room Name"
                        }
                      },
                      {
                        "sender": "@bob:example.com",
                        "type": "m.room.member",
                        "state_key": "@bob:example.com",
                        "content": {
                          "membership": "knock"
                        }
                      }
                    ]
                  }
                }
            },
            "leave": {
                *MIXED_LEFT_ROOM_ID: {
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "membership": "leave"
                                },
                                "origin_server_ts": 158957809000000_u64,
                                "sender": "@example:localhost",
                                "state_key": "@example:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "replaces_state": "$blahblah",
                                    "prev_content": {
                                        "avatar_url": null,
                                        "displayname": "me",
                                        "membership": "invite"
                                    },
                                    "prev_sender": "@2example:localhost",
                                    "age": 1757
                                },
                                "event_id": "$lQQ116Y-XqcjpSUGpuz36rNntUvOSpTjuaIvmtQ2AwA"
                            }
                        ],
                        "prev_batch": "toktok",
                        "limited": false
                    },
                    "state": {
                        "events": []
                    },
                    "account_data": {
                        "events": []
                    }
                }
            }
        },
        "groups": {
            "join": {},
            "invite": {},
            "leave": {}
        },
        "device_one_time_keys_count": {
            "signed_curve25519": 50
        },
        "next_batch": "s1380317562_757269739_1655566_503953763_334052043_1209862_55290918_65705002_101146"
    })
});

pub static VOIP_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {},
                    "account_data": {
                        "events": []
                    },
                    "ephemeral": {
                        "events": [ ]
                    },
                    "state": {
                        "events": []
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "call_id": "12345",
                                    "lifetime": 60000,
                                    "offer": {
                                        "sdp": "v=0\r\no=- 6584580628695956864 2 IN IP4 127.0.0.1[...]",
                                        "type": "offer"
                                    },
                                    "version": 0
                                },
                                "event_id": "$143273582443PhrSn:example.org",
                                "origin_server_ts": 143273582000000_u64,
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "sender": "@example:example.org",
                                "type": "m.call.invite",
                                "unsigned": {
                                    "age": 1234
                                }
                            },
                            {
                                "content": {
                                    "answer": {
                                        "sdp": "v=0\r\no=- 6584580628695956864 2 IN IP4 127.0.0.1[...]",
                                        "type": "answer"
                                    },
                                    "call_id": "12345",
                                    "lifetime": 60000,
                                    "version": 0
                                },
                                "event_id": "$143273582443PhrSn:example.org",
                                "origin_server_ts": 143273582000000_u64,
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "sender": "@example:example.org",
                                "type": "m.call.answer",
                                "unsigned": {
                                    "age": 1234
                                }
                            },
                            {
                                "content": {
                                    "call_id": "12345",
                                    "candidates": [
                                        {
                                            "candidate": "candidate:863018703 1 udp 2122260223 10.9.64.156 43670 type host generation 0",
                                            "sdpMLineIndex": 0,
                                            "sdpMid": "audio"
                                        }
                                    ],
                                    "version": 0
                                },
                                "event_id": "$143273582443PhrSn:example.org",
                                "origin_server_ts": 143273582000000_u64,
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "sender": "@example:example.org",
                                "type": "m.call.candidates",
                                "unsigned": {
                                    "age": 1234
                                }
                            },
                            {
                                "content": {
                                    "call_id": "12345",
                                    "version": 0
                                },
                                "event_id": "$143273582443PhrSn:example.org",
                                "origin_server_ts": 143273582000000_u64,
                                "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                "sender": "@example:example.org",
                                "type": "m.call.hangup",
                                "unsigned": {
                                    "age": 1234
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": []
        }
    })
});

pub static SYNC_ADMIN_AND_MOD: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@admin:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {
                        "m.heroes": [
                          "@example2:localhost"
                        ],
                        "m.joined_member_count": 2,
                        "m.invited_member_count": 0
                      },
                    "account_data": {
                        "events": []
                    },
                    "ephemeral": {
                        "events": []
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "admin",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "@admin:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 297036,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "mod",
                                    "membership": "join"
                                },
                                "event_id": "$151800140518rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 1518001450000000_u64,
                                "sender": "@mod:localhost",
                                "state_key": "@mod:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 297035,
                                }
                            },
                            {
                                "content": {
                                    "history_visibility": "shared"
                                },
                                "event_id": "$15139375515VaJEY:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.history_visibility",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "topic": "room topic"
                                },
                                "event_id": "$151957878228ssqrJ:localhost",
                                "origin_server_ts": 151957878000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.topic",
                                "unsigned": {
                                    "age": 1392989709,
                                    "prev_content": {
                                        "topic": "test"
                                    },
                                    "prev_sender": "@example:localhost",
                                    "replaces_state": "$151957069225EVYKm:localhost"
                                }
                            },
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@admin:localhost": 100,
                                        "@mod:localhost": 50
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            }
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@admin:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": []
        }
    })
});

pub static CUSTOM_ROOM_POWER_LEVELS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
        "device_lists": {
            "changed": [
                "@admin:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
                    "summary": {
                        "m.heroes": [
                          "@example2:localhost"
                        ],
                        "m.joined_member_count": 1,
                        "m.invited_member_count": 0
                      },
                    "account_data": {
                        "events": []
                    },
                    "ephemeral": {
                        "events": []
                    },
                    "state": {
                        "events": [
                            {
                                "content": {
                                    "join_rule": "public"
                                },
                                "event_id": "$15139375514WsgmR:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.join_rules",
                                "unsigned": {
                                    "age": 7034220
                                }
                            },
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "admin",
                                    "membership": "join"
                                },
                                "event_id": "$151800140517rfvjc:localhost",
                                "membership": "join",
                                "origin_server_ts": 151800140000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "@admin:localhost",
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 297036,
                                    "replaces_state": "$151800111315tsynI:localhost"
                                }
                            },
                            {
                                "content": {
                                    "creator": "@example:localhost"
                                },
                                "event_id": "$15139375510KUZHi:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.create",
                                "unsigned": {
                                    "age": 703422
                                }
                            },
                            {
                                "content": {
                                    "ban": 100,
                                    "events": {
                                        "m.room.avatar": 100,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@admin:localhost": 100
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755000000_u64,
                                "sender": "@admin:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            }
                        ]
                    },
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "body": "baba",
                                    "format": "org.matrix.custom.html",
                                    "formatted_body": "<strong>baba</strong>",
                                    "msgtype": "m.text"
                                },
                                "event_id": "$152037280074GZeOm:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@admin:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 598971425
                                }
                            }
                        ],
                        "limited": true,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
                    }
                }
            },
            "leave": {}
        },
        "to_device": {
            "events": []
        },
        "presence": {
            "events": []
        }
    })
});
