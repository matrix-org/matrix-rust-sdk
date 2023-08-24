pub use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher as _};
use matrix_sdk::{Client, RoomListEntry};

use super::normalize_string;

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

    fn fuzzy_match(&self, subject: &str) -> bool {
        // No pattern means there is a match.
        let Some(pattern) = self.pattern.as_ref() else { return true };

        self.matcher.fuzzy_match(&normalize_string(subject), pattern).is_some()
    }
}

/// Create a new filter that will fuzzy match a pattern on room names.
///
/// Rooms are fetched from the `Client`. The pattern and the room names are
/// normalized with `normalize_string`.
pub fn new_filter(client: &Client, pattern: &str) -> impl Fn(&RoomListEntry) -> bool {
    let searcher = FuzzyMatcher::new().with_pattern(pattern);

    let client = client.clone();

    move |room_list_entry| -> bool {
        let Some(room_id) = room_list_entry.as_room_id() else { return false };
        let Some(room) = client.get_room(room_id) else { return false };
        let Some(room_name) = room.name() else { return false };

        searcher.fuzzy_match(&room_name)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::FuzzyMatcher;

    #[test]
    fn test_no_pattern() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.fuzzy_match("hello"));
    }

    #[test]
    fn test_literal() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.fuzzy_match("matrix"));

        let matcher = matcher.with_pattern("mxt");
        assert!(matcher.fuzzy_match("matrix").not());
    }

    #[test]
    fn test_ignore_case() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.fuzzy_match("MaTrIX"));

        let matcher = matcher.with_pattern("mxt");
        assert!(matcher.fuzzy_match("MaTrIX").not());
    }

    #[test]
    fn test_smart_case() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("mtx");
        assert!(matcher.fuzzy_match("matrix"));
        assert!(matcher.fuzzy_match("Matrix"));

        let matcher = matcher.with_pattern("Mtx");
        assert!(matcher.fuzzy_match("matrix").not());
        assert!(matcher.fuzzy_match("Matrix"));
    }

    #[test]
    fn test_normalization() {
        let matcher = FuzzyMatcher::new();

        let matcher = matcher.with_pattern("ubété");

        // First, assert that the pattern has been normalized.
        assert_eq!(matcher.pattern, Some("ubete".to_owned()));

        // Second, assert that the subject is normalized too.
        assert!(matcher.fuzzy_match("un bel été"));

        // Another concrete test.
        let matcher = matcher.with_pattern("stf");
        assert!(matcher.fuzzy_match("Ștefan"));
    }
}
