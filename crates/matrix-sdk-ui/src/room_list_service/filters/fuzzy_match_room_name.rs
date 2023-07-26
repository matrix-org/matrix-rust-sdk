pub use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher as _};
use matrix_sdk::{Client, RoomListEntry};

struct FuzzyMatcher {
    matcher: SkimMatcherV2,
}

impl FuzzyMatcher {
    fn new() -> Self {
        Self { matcher: SkimMatcherV2::default().smart_case().use_cache(true) }
    }

    fn fuzzy_match(&self, subject: &str, pattern: &str) -> bool {
        self.matcher.fuzzy_match(subject, pattern).is_some()
    }
}

pub fn new_filter(
    client: &Client,
    pattern: String,
) -> impl Fn(&RoomListEntry) -> bool + Send + Sync + 'static {
    let searcher = FuzzyMatcher::new();
    let client = client.clone();

    move |room_list_entry| -> bool {
        let Some(room_id) = room_list_entry.as_room_id() else { return false };
        let Some(room) = client.get_room(room_id) else { return false };
        let Some(room_name) = room.name() else { return false };

        searcher.fuzzy_match(&room_name, &pattern)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::*;

    #[test]
    fn test_literal() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.fuzzy_match("matrix", "mtx"));
        assert!(matcher.fuzzy_match("matrix", "mxt").not());
    }

    #[test]
    fn test_ignore_case() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.fuzzy_match("MaTrIX", "mtx"));
        assert!(matcher.fuzzy_match("MaTrIX", "mxt").not());
    }

    #[test]
    fn test_smart_case() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.fuzzy_match("Matrix", "mtx"));
        assert!(matcher.fuzzy_match("Matrix", "mtx"));
        assert!(matcher.fuzzy_match("MatriX", "Mtx").not());
    }

    // This is not supported yet.
    /*
    #[test]
    fn test_transliteration_and_normalization() {
        let matcher = FuzzyMatcher::new();

        assert!(matcher.fuzzy_match("un bel été", "été"));
        assert!(matcher.fuzzy_match("un bel été", "ete"));
        assert!(matcher.fuzzy_match("un bel été", "éte"));
        assert!(matcher.fuzzy_match("un bel été", "étè").not());
        assert!(matcher.fuzzy_match("Ștefan", "stef"));
    }
    */
}
