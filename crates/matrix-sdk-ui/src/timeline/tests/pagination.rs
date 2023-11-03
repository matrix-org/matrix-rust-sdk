// Copyright 2023 Kévin Commaille
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

use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_test::async_test;
use ruma::serde::Raw;
use serde_json::json;

use super::TestTimeline;
use crate::timeline::pagination::PaginationTokens;

#[async_test]
async fn empty_chunk() {
    let timeline = TestTimeline::new();

    timeline
        .handle_back_paginated_events(
            vec![],
            PaginationTokens { from: None, to: Some("a".to_owned()) },
        )
        .await;

    timeline
        .handle_back_paginated_events(
            vec![],
            PaginationTokens { from: Some("a".to_owned()), to: Some("b".to_owned()) },
        )
        .await;
}

#[async_test]
async fn invalid_event() {
    let timeline = TestTimeline::new();

    // Invalid empty event.
    let raw = Raw::new(&json!({})).unwrap();

    timeline
        .handle_back_paginated_events(
            vec![TimelineEvent::new(raw.cast())],
            PaginationTokens { from: None, to: Some("a".to_owned()) },
        )
        .await;

    timeline
        .handle_back_paginated_events(
            vec![],
            PaginationTokens { from: Some("a".to_owned()), to: Some("b".to_owned()) },
        )
        .await;
}
