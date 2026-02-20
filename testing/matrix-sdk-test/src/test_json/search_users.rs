use std::sync::LazyLock;

use serde_json::{Value as JsonValue, json};

pub static SEARCH_USERS_REQUEST: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "search_term": "test",
        "limit": 50
    })
});

pub static SEARCH_USERS_RESPONSE: LazyLock<JsonValue> = LazyLock::new(|| {
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
