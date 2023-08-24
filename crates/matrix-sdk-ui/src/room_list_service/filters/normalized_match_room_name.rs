use matrix_sdk::{Client, RoomListEntry};

use super::normalize_string;

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

    fn normalized_match(&self, subject: &str) -> bool {
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
pub fn new_filter(client: &Client, pattern: &str) -> impl Fn(&RoomListEntry) -> bool {
    let searcher = NormalizedMatcher::new().with_pattern(pattern);

    let client = client.clone();

    move |room_list_entry| -> bool {
        let Some(room_id) = room_list_entry.as_room_id() else { return false };
        let Some(room) = client.get_room(room_id) else { return false };
        let Some(room_name) = room.name() else { return false };

        searcher.normalized_match(&room_name)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::NormalizedMatcher;

    #[test]
    fn test_no_pattern() {
        let matcher = NormalizedMatcher::new();

        assert!(matcher.normalized_match("hello"));
    }

    #[test]
    fn test_literal() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("matrix");
        assert!(matcher.normalized_match("matrix"));

        let matcher = matcher.with_pattern("matrxi");
        assert!(matcher.normalized_match("matrix").not());
    }

    #[test]
    fn test_ignore_case() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("matrix");
        assert!(matcher.normalized_match("MaTrIX"));

        let matcher = matcher.with_pattern("matrxi");
        assert!(matcher.normalized_match("MaTrIX").not());
    }

    #[test]
    fn test_normalization() {
        let matcher = NormalizedMatcher::new();

        let matcher = matcher.with_pattern("un été");

        // First, assert that the pattern has been normalized.
        assert_eq!(matcher.pattern, Some("un ete".to_owned()));

        // Second, assert that the subject is normalized too.
        assert!(matcher.normalized_match("un été magnifique"));

        // Another concrete test.
        let matcher = matcher.with_pattern("stefan");
        assert!(matcher.normalized_match("Ștefan"));
    }
}
