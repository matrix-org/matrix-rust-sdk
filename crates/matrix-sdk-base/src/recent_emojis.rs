//! Data types used for handling the recently used emojis.
//!
//! There is no formal spec for this, only the implementation in Element Web:
//! <https://github.com/element-hq/element-web/commit/a7f92f35f5a27a53a5a030ea7c471be97751a67a>

use ruma::{UInt, events::macros::EventContent};
use serde::{Deserialize, Serialize};

/// An event type containing a list of recently used emojis for reactions.
#[cfg(feature = "experimental-element-recent-emojis")]
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "io.element.recent_emoji", kind = GlobalAccountData)]
pub struct RecentEmojisContent {
    /// The list of recently used emojis, ordered by recency. The tuple of
    /// `String`, `UInt` values represent the actual emoji and the number of
    /// times it's been used in total, for those clients that might be
    /// interested.
    pub recent_emoji: Vec<(String, UInt)>,
}

#[cfg(feature = "experimental-element-recent-emojis")]
impl RecentEmojisContent {
    /// Creates a new recent emojis event content given the provided recent
    /// emojis.
    pub fn new(recent_emoji: Vec<(String, UInt)>) -> Self {
        Self { recent_emoji }
    }
}

#[cfg(feature = "experimental-element-recent-emojis")]
#[cfg(test)]
mod tests {
    use ruma::uint;
    use serde_json::{from_value, json, to_value};

    use crate::recent_emojis::RecentEmojisContent;

    #[test]
    fn serialization() {
        let content = RecentEmojisContent::new(vec![
            ("😁".to_owned(), uint!(2)),
            ("🎉".to_owned(), uint!(10)),
        ]);
        let json = to_value(&content).expect("recent emoji serialization failed");
        let expected = json!({
            "recent_emoji": [
                ["😁", 2],
                ["🎉", 10],
            ]
        });

        assert_eq!(json, expected);
    }

    #[test]
    fn deserialization() {
        let json = json!({
            "recent_emoji": [
                ["😁", 2],
                ["🎉", 10],
            ]
        });
        let content =
            from_value::<RecentEmojisContent>(json).expect("recent emoji deserialization failed");
        let expected = RecentEmojisContent::new(vec![
            ("😁".to_owned(), uint!(2)),
            ("🎉".to_owned(), uint!(10)),
        ]);

        assert_eq!(content.recent_emoji, expected.recent_emoji);
    }
}
