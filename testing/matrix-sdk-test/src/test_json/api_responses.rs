//! Responses to client API calls.

use once_cell::sync::Lazy;
use serde_json::{json, Value as JsonValue};

/// `GET /_matrix/client/v3/devices`
pub static DEVICES: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "devices": [
            {
                "device_id": "BNYQQWUMXO",
                "display_name": "Client 1",
                "last_seen_ip": "-",
                "last_seen_ts": 1596117733037u64,
                "user_id": "@example:localhost"
            },
            {
                "device_id": "LEBKSEUSNR",
                "display_name": "Client 2",
                "last_seen_ip": "-",
                "last_seen_ts": 1599057006985u64,
                "user_id": "@example:localhost"
            }
        ]
    })
});

/// `GET /_matrix/client/v3/directory/room/{roomAlias}`
pub static GET_ALIAS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "room_id": "!lUbmUPdxdXxEQurqOs:example.net",
        "servers": [
          "example.org",
          "example.net",
          "matrix.org",
        ]
    })
});

/// `POST /_matrix/client/v3/keys/query`
pub static KEYS_QUERY: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "device_keys": {
        "@alice:example.org": {
          "JLAFKJWSCS": {
              "algorithms": [
                  "m.olm.v1.curve25519-aes-sha2",
                  "m.megolm.v1.aes-sha2"
              ],
              "device_id": "JLAFKJWSCS",
              "user_id": "@alice:example.org",
              "keys": {
                  "curve25519:JLAFKJWSCS": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
                  "ed25519:JLAFKJWSCS": "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM"
              },
              "signatures": {
                  "@alice:example.org": {
                      "ed25519:JLAFKJWSCS": "m53Wkbh2HXkc3vFApZvCrfXcX3AI51GsDHustMhKwlv3TuOJMj4wistcOTM8q2+e/Ro7rWFUb9ZfnNbwptSUBA"
                  }
              },
              "unsigned": {
                  "device_display_name": "Alice's mobile phone"
              }
          }
        }
      },
      "failures": {}
    })
});

/// `POST /_matrix/client/v3/keys/query`
/// For a set of 2 devices own by a user named web2.
/// First device is unsigned, second one is signed
pub static KEYS_QUERY_TWO_DEVICES_ONE_SIGNED: Lazy<JsonValue> = Lazy::new(|| {
    json!({
    "device_keys":{
       "@web2:localhost:8482":{
          "AVXFQWJUQA":{
             "algorithms":[
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
             ],
             "device_id":"AVXFQWJUQA",
             "keys":{
                "curve25519:AVXFQWJUQA":"LTpv2DGMhggPAXO02+7f68CNEp6A40F0Yl8B094Y8gc",
                "ed25519:AVXFQWJUQA":"loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw"
             },
             "signatures":{
                "@web2:localhost:8482":{
                   "ed25519:AVXFQWJUQA":"BmdzjXMwZaZ0ZK8T6h3pkTA+gZbD34Bzf8FNazBdAIE16fxVzrlSJkLfXnjdBqRO0Dlda5vKgGpqJazZP6obDw"
                }
             },
             "user_id":"@web2:localhost:8482"
          },
          "JERTCKWUWG":{
             "algorithms":[
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
             ],
             "device_id":"JERTCKWUWG",
             "keys":{
                "curve25519:JERTCKWUWG":"XJixbpnfIk+RqcK5T6moqVY9d9Q1veR8WjjSlNiQNT0",
                "ed25519:JERTCKWUWG":"48f3WQAMGwYLBg5M5qUhqnEVA8yeibjZpPsShoWMFT8"
             },
             "signatures":{
                "@web2:localhost:8482":{
                   "ed25519:JERTCKWUWG":"Wc67XYem4IKCpshcslQ6ketCE5otubpX+Bh01OB8ghLxl1d6exlZsgaRA57N8RJ0EMvbeTWCweHXXC/UeeQ4DQ",
                   "ed25519:uXOM0Xlfts9SGysk/yNr0Vn9rgv1Ifh3R8oPhtic4BM":"dto9VPhhJbNw62j8NQyjnwukMd1NtYnDYSoUOzD5dABq1u2Kt/ZdthcTO42HyxG/3/hZdno8XPfJ47l1ZxuXBA"
                }
             },
             "user_id":"@web2:localhost:8482"
          }
       }
    },
    "failures":{

    },
    "master_keys":{
       "@web2:localhost:8482":{
          "user_id":"@web2:localhost:8482",
          "usage":[
             "master"
          ],
          "keys":{
             "ed25519:Ct4QR+aXrzW4iYIgH1B/56NkPEtSPoN+h2TGoQ0xxYI":"Ct4QR+aXrzW4iYIgH1B/56NkPEtSPoN+h2TGoQ0xxYI"
          },
          "signatures":{
             "@web2:localhost:8482":{
                "ed25519:JERTCKWUWG":"H9hEsUJ+alB5XAboDzU4loVb+SZajC4tsQzGaeU/FHMFAnWeVarTMCR+NmPSGsZfvPrNz2WVS2G7FIH5yhJfBg"
             }
          }
       }
    },
    "self_signing_keys":{
       "@web2:localhost:8482":{
          "user_id":"@web2:localhost:8482",
          "usage":[
             "self_signing"
          ],
          "keys":{
             "ed25519:uXOM0Xlfts9SGysk/yNr0Vn9rgv1Ifh3R8oPhtic4BM":"uXOM0Xlfts9SGysk/yNr0Vn9rgv1Ifh3R8oPhtic4BM"
          },
          "signatures":{
             "@web2:localhost:8482":{
                "ed25519:Ct4QR+aXrzW4iYIgH1B/56NkPEtSPoN+h2TGoQ0xxYI":"YbD6gTEwY078nllTxmlyea2VNvAElQ/ig7aPsyhA3h1gGwFvPdtyDbomjdIphUF/lXQ+Eyz4SzlUWeghr1b3BA"
             }
          }
       }
    },
    "user_signing_keys":{

    }
     })
});

/// ``
pub static KEYS_UPLOAD: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "one_time_key_counts": {
        "curve25519": 10,
        "signed_curve25519": 20
      }
    })
});

/// Successful call to `POST /_matrix/client/v3/login` without auto-discovery.
pub static LOGIN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "access_token": "abc123",
        "device_id": "GHTYAJCE",
        "home_server": "matrix.org",
        "user_id": "@cheeky_monkey:matrix.org"
    })
});

/// Successful call to `POST /_matrix/client/v3/login` with auto-discovery.
pub static LOGIN_WITH_DISCOVERY: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "access_token": "abc123",
        "device_id": "GHTYAJCE",
        "home_server": "matrix.org",
        "user_id": "@cheeky_monkey:matrix.org",
        "well_known": {
            "m.homeserver": {
                "base_url": "https://example.org"
            },
            "m.identity_server": {
                "base_url": "https://id.example.org"
            }
        }
    })
});

/// Successful call to `POST /_matrix/client/v3/login` with a refresh token.
pub static LOGIN_WITH_REFRESH_TOKEN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "access_token": "abc123",
        "device_id": "GHTYAJCE",
        "home_server": "matrix.org",
        "user_id": "@cheeky_monkey:matrix.org",
        "expires_in_ms": 432000000,
        "refresh_token": "zyx987",
    })
});

/// Failed call to `POST /_matrix/client/v3/login`
pub static LOGIN_RESPONSE_ERR: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "errcode": "M_FORBIDDEN",
      "error": "Invalid password"
    })
});

/// `GET /_matrix/client/v3/login`
pub static LOGIN_TYPES: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "flows": [
            {
                "type": "m.login.password"
            },
            {
                "type": "m.login.sso"
            },
            {
                "type": "m.login.token"
            }
        ]
    })
});

/// Failed call to an endpoint when the resource that was asked could not be
/// found.
pub static NOT_FOUND: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "errcode": "M_NOT_FOUND",
      "error": "No resource was found for this request.",
      "soft_logout": true,
    })
});

/// `GET /_matrix/client/v3/publicRooms`
pub static PUBLIC_ROOMS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "chunk": [
            {
                "aliases": [
                    "#murrays:cheese.bar"
                ],
                "avatar_url": "mxc://bleeker.street/CHEDDARandBRIE",
                "guest_can_join": false,
                "name": "CHEESE",
                "num_joined_members": 37,
                "room_id": "!ol19s:bleecker.street",
                "topic": "Tasty tasty cheese",
                "world_readable": true
            }
        ],
        "next_batch": "p190q",
        "prev_batch": "p1902",
        "total_room_count_estimate": 115
    })
});

/// `POST /_matrix/client/v3/refresh` without new refresh token.
pub static REFRESH_TOKEN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "access_token": "5678",
      "expire_in_ms": 432000000,
    })
});

/// `POST /_matrix/client/v3/refresh` with a new refresh token.
pub static REFRESH_TOKEN_WITH_REFRESH_TOKEN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "access_token": "9012",
      "expire_in_ms": 432000000,
      "refresh_token": "wxyz",
    })
});

/// Failed call to `POST /_matrix/client/v3/register`
pub static REGISTRATION_RESPONSE_ERR: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "errcode": "M_FORBIDDEN",
        "error": "Invalid password",
        "completed": ["example.type.foo"],
        "flows": [
            {
                "stages": ["example.type.foo", "example.type.bar"]
            },
            {
                "stages": ["example.type.foo", "example.type.baz"]
            }
        ],
        "params": {
            "example.type.baz": {
                "example_key": "foobar"
            }
        },
        "session": "xxxxxx"
    })
});

/// Failed called to any endpoint with an expired access token.
pub static UNKNOWN_TOKEN_SOFT_LOGOUT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
      "errcode": "M_UNKNOWN_TOKEN",
      "error": "Invalid access token passed.",
      "soft_logout": true,
    })
});

/// `GET /_matrix/client/versions`
pub static VERSIONS: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "versions": [
            "r0.0.1",
            "r0.1.0",
            "r0.2.0",
            "r0.3.0",
            "r0.4.0",
            "r0.5.0",
            "r0.6.0"
        ],
        "unstable_features": {
            "org.matrix.label_based_filtering":true,
            "org.matrix.e2e_cross_signing":true
        }
    })
});

/// `GET /.well-known/matrix/client`
pub static WELL_KNOWN: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "m.homeserver": {
            "base_url": "HOMESERVER_URL"
        }
    })
});

/// `GET /_matrix/client/v3/account/whoami`
pub static WHOAMI: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "user_id": "@joe:example.org"
    })
});
