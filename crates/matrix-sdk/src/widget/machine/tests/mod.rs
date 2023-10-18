// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use assert_matches::assert_matches;
use ruma::serde::JsonObject;
use serde_json::Value as JsonValue;

/// Create a JSON string from a [`json!`][serde_json::json] "literal".
#[macro_export]
macro_rules! json_string {
    ($( $tt:tt )*) => { ::serde_json::json!( $($tt)* ).to_string() };
}

mod capabilities;
mod error;

const WIDGET_ID: &str = "test-widget";

fn parse_msg(msg: &str) -> (JsonValue, String) {
    let mut deserialized: JsonObject = serde_json::from_str(msg).unwrap();
    let request_id =
        assert_matches!(deserialized.remove("requestId").unwrap(), JsonValue::String(s) => s);
    (JsonValue::Object(deserialized), request_id)
}
