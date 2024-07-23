use serde::{Deserialize, Serialize};

use super::SlidingSyncList;

#[derive(Debug, Serialize, Deserialize)]
pub struct FrozenSlidingSyncList {
    #[serde(default, rename = "rooms_count", skip_serializing_if = "Option::is_none")]
    pub maximum_number_of_rooms: Option<u32>,
}

impl FrozenSlidingSyncList {
    pub(in super::super) fn freeze(source_list: &SlidingSyncList) -> Self {
        FrozenSlidingSyncList { maximum_number_of_rooms: source_list.maximum_number_of_rooms() }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::FrozenSlidingSyncList;

    #[test]
    fn test_frozen_sliding_sync_list_serialization() {
        assert_eq!(
            serde_json::to_value(&FrozenSlidingSyncList { maximum_number_of_rooms: Some(42) })
                .unwrap(),
            json!({
                "rooms_count": 42,
            })
        );
    }
}
