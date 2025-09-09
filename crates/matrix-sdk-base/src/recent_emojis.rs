//! Data types used for handling the recently used emojis.

use ruma::{UInt, events::macros::EventContent};
use serde::{Deserialize, Serialize};

#[cfg(feature = "experimental-element-recent-emojis")]
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "io.element.recent_emoji", kind = GlobalAccountData)]
/// An event type containing a list of recently used emojis for reactions.
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
