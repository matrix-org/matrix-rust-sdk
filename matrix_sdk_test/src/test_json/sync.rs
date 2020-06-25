use lazy_static::lazy_static;
use serde_json::{json, Value as JsonValue};

lazy_static! {
    pub static ref SYNC: JsonValue = json!({
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
                "!SVkFJHzfwvuaIEawgC:localhost": {
                    "summary": {},
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
                                "room_id": "!SVkFJHzfwvuaIEawgC:localhost",
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151800140,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151957878,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 152034824,
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
                                "origin_server_ts": 158534550,
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
                                "origin_server_ts": 152037280,
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
                        "avatar_url": "mxc://localhost:wefuiwegh8742w",
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
    });
}

lazy_static! {
    pub static ref DEFAULT_SYNC_SUMMARY: JsonValue = json!({
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
                "!SVkFJHzfwvuaIEawgC:localhost": {
                    "summary": {
                        "m.heroes": [
                          "@alice:example.com",
                          "@bob:example.com"
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151800140,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151957878,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 152034824,
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
                                "origin_server_ts": 158534550,
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
                                "origin_server_ts": 152037280,
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
                        "avatar_url": "mxc://localhost:wefuiwegh8742w",
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
    });
}

lazy_static! {
    pub static ref MORE_SYNC: JsonValue = json!({
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
                "!SVkFJHzfwvuaIEawgC:localhost": {
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
                                "origin_server_ts": 152037280,
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
                                        "event_id": "some event id",
                                        "rel_type": "m.replace"
                                    },
                                    "msgtype": "m.text"
                                },
                                "event_id": "edit event id",
                                "origin_server_ts": 159026265,
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
                                "origin_server_ts": 151957878,
                                "sender": "@example:localhost",
                                "type": "m.room.redaction",
                                "redacts": "$151957878228ssqrj:localhost"
                            },
                            {
                                "content": {},
                                "event_id": "$15275046980maRLj:localhost",
                                "origin_server_ts": 152750469,
                                "sender": "@example:localhost",
                                "type": "m.room.message",
                                "unsigned": {
                                    "age": 19334,
                                    "redacted_because": {
                                        "content": {},
                                        "event_id": "$15275047031IXQRi:localhost",
                                        "origin_server_ts": 152750470,
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
                                        "event_id": "some event id",
                                        "key": "👍",
                                        "rel_type": "m.annotation"
                                    }
                                },
                                "event_id": "event id",
                                "origin_server_ts": 159027581,
                                "sender": "@alice:matrix.org",
                                "type": "m.reaction",
                                "unsigned": {
                                    "age": 85
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
    });
}

lazy_static! {
    pub static ref INVITE_SYNC: JsonValue = json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_1",
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
                        "avatar_url": "mxc://localhost:wefuiwegh8742w",
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
    });
}

lazy_static! {
    pub static ref LEAVE_SYNC: JsonValue = json!({
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
            "join": {},
            "leave": {
                "!SVkFJHzfwvuaIEawgC:localhost": {
                    "summary": {},
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151800140,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151957878,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 151393755,
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
                                "origin_server_ts": 152034824,
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
                                "origin_server_ts": 158534550,
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
                                "origin_server_ts": 152037280,
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
                        "avatar_url": "mxc://localhost:wefuiwegh8742w",
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
    });
}

lazy_static! {
    pub static ref LEAVE_SYNC_EVENT: JsonValue = json!({
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
                "!SVkFJHzfwvuaIEawgC:localhost": {
                    "timeline": {
                        "events": [
                            {
                                "content": {
                                    "membership": "leave"
                                },
                                "origin_server_ts": 158957809,
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
    });
}
