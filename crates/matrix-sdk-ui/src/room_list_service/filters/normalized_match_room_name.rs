// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use tracing::error;

use super::{Filter, normalize_string};

struct NormalizedMatcher {
    pattern: Option<String>,
}

impl NormalizedMatcher {
    fn new() -> Self {
        Self { pattern: None }
    }

    fn with_pattern(mut self, pattern: &str) -> Self {
        self.pattern = Some(normalize_string(&pattern.to_lowercase()));

        self
    }

    fn matches(&self, subject: &str) -> bool {
        // No pattern means there is a match.
        let Some(pattern) = self.pattern.as_ref() else { return true };

        let subject = normalize_string(&subject.to_lowercase());

        subject.contains(pattern)
    }
}

/// Create a new filter that will “normalized” match a pattern on room names.
///
/// Rooms are fetched from the `Client`. The pattern and the room names are
/// normalized with `normalize_string`.
pub fn new_filter(pattern: &str) -> impl Filter + use<> {
    let searcher = NormalizedMatcher::new().with_pattern(pattern);

    move |room| -> bool {
        let Some(room_name) = room.cached_display_name() else {
            error!(room_id = ?room.room_id(), "Missing cached room display name");

            return false;
        };

        searcher.matches(&room_name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::*;

    #[test]
    fn test_no_pattern() {
        let matcher = NormalizedMatcher::new();

        assert!(matcher.matches("hello"));
    }

    #[test]
    fn test_empty_pattern() {
        let matcher = NormalizedMatcher::new();

        assert!(matcher.matches("hello"));
    }

    #[test]
    fn test_literal() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("matrix");
        assert!(matcher.matches("matrix"));

        let matcher = matcher.with_pattern("matrxi");
        assert!(matcher.matches("matrix").not());
    }

    #[test]
    fn test_ignore_case() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("matrix");
        assert!(matcher.matches("MaTrIX"));

        let matcher = matcher.with_pattern("matrxi");
        assert!(matcher.matches("MaTrIX").not());
    }

    #[test]
    fn test_normalization() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("un été");

        // First, assert that the pattern has been normalized.
        assert_eq!(matcher.pattern, Some("un ete".to_owned()));

        // Second, assert that the subject is normalized too.
        assert!(matcher.matches("un été magnifique"));

        // Another concrete test.
        let matcher = matcher.with_pattern("stefan");
        assert!(matcher.matches("Ștefan"));
    }
}
