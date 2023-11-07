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

use assert_matches2::assert_matches;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_test::async_test;
use ruma::serde::Raw;
use serde_json::json;

use super::TestTimeline;
use crate::timeline::{inner::HandleBackPaginatedEventsError, pagination::PaginationTokens};

#[async_test]
async fn back_pagination_token_not_updated_with_empty_chunk() {
    let timeline = TestTimeline::new();

    timeline
        .inner
        .handle_back_paginated_events(
            vec![],
            PaginationTokens { from: None, check_from: true, to: Some("a".to_owned()) },
        )
        .await
        .unwrap();

    // Checking the token fails because it has not been updated.
    let err = timeline
        .inner
        .handle_back_paginated_events(
            vec![],
            PaginationTokens {
                from: Some("a".to_owned()),
                check_from: true,
                to: Some("b".to_owned()),
            },
        )
        .await
        .unwrap_err();
    assert_matches!(err, HandleBackPaginatedEventsError::TokenMismatch);

    // Not checking the token works.
    timeline
        .inner
        .handle_back_paginated_events(
            vec![],
            PaginationTokens {
                from: Some("a".to_owned()),
                check_from: false,
                to: Some("b".to_owned()),
            },
        )
        .await
        .unwrap();
}

#[async_test]
async fn back_pagination_token_not_updated_invalid_event() {
    let timeline = TestTimeline::new();

    // Invalid empty event.
    let raw = Raw::new(&json!({})).unwrap();

    timeline
        .inner
        .handle_back_paginated_events(
            vec![TimelineEvent::new(raw.cast())],
            PaginationTokens { from: None, check_from: true, to: Some("a".to_owned()) },
        )
        .await
        .unwrap();

    // Checking the token fails because it has not been updated.
    let err = timeline
        .inner
        .handle_back_paginated_events(
            vec![],
            PaginationTokens {
                from: Some("a".to_owned()),
                check_from: true,
                to: Some("b".to_owned()),
            },
        )
        .await
        .unwrap_err();
    assert_matches!(err, HandleBackPaginatedEventsError::TokenMismatch);

    // Not checking the token works.
    timeline
        .inner
        .handle_back_paginated_events(
            vec![],
            PaginationTokens {
                from: Some("a".to_owned()),
                check_from: false,
                to: Some("b".to_owned()),
            },
        )
        .await
        .unwrap();
}
