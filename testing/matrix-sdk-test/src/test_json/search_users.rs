use once_cell::sync::Lazy;
use serde_json::{Value as JsonValue, json};

pub static SEARCH_USERS_REQUEST: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "search_term": "test",
        "limit": 50
    })
});

pub static SEARCH_USERS_RESPONSE: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "limited": false,
        "results": [
            {
                "user_id": "@test:example.me",
                "display_name": "Test",
                "avatar_url": "mxc://example.me/someid"
            }
        ]
    })
});
