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

pub use fuzzy_matcher::{FuzzyMatcher as _, skim::SkimMatcherV2};

use super::{Filter, normalize_string};

struct FuzzyMatcher {
    matcher: SkimMatcherV2,
    pattern: Option<String>,
}

impl FuzzyMatcher {
    fn new() -> Self {
        Self { matcher: SkimMatcherV2::default().smart_case().use_cache(true), pattern: None }
    }

    fn with_pattern(mut self, pattern: &str) -> Self {
        self.pattern = Some(normalize_string(pattern));

        self
    }

    fn matches(&self, subject: &str) -> bool {
        // No pattern means there is a match.
        let Some(pattern) = self.pattern.as_ref() else { return true };

        self.matcher.fuzzy_match(&normalize_string(subject), pattern).is_some()
    }
}

/// Create a new filter that will fuzzy match a pattern on room names.
///
/// Rooms are fetched from the `Client`. The pattern and the room names are
/// normalized with `normalize_string`.
pub fn new_filter(pattern: &str) -> impl Filter + use<> {
    let searcher = FuzzyMatcher::new().with_pattern(pattern);

    move |room| -> bool {
        let Some(room_name) = &room.cached_display_name else { return false };

        searcher.matches(room_name)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::*;

    #[test]
    fn test_no_pattern() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.matches("hello"));
    }

    #[test]
    fn test_empty_pattern() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.matches("hello"));
    }

    #[test]
    fn test_literal() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.matches("matrix"));

        let matcher = matcher.with_pattern("mxt");
        assert!(matcher.matches("matrix").not());
    }

    #[test]
    fn test_ignore_case() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.matches("MaTrIX"));

        let matcher = matcher.with_pattern("mxt");
        assert!(matcher.matches("MaTrIX").not());
    }

    #[test]
    fn test_smart_case() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.matches("matrix"));
        assert!(matcher.matches("Matrix"));

        let matcher = matcher.with_pattern("Mtx");
        assert!(matcher.matches("matrix").not());
        assert!(matcher.matches("Matrix"));
    }

    #[test]
    fn test_normalization() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("ubété");

        // First, assert that the pattern has been normalized.
        assert_eq!(matcher.pattern, Some("ubete".to_owned()));

        // Second, assert that the subject is normalized too.
        assert!(matcher.matches("un bel été"));

        // Another concrete test.
        let matcher = matcher.with_pattern("stf");
        assert!(matcher.matches("Ștefan"));
    }
}
