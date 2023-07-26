use ruma::{OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};

/// Represent a room entry in the [`SlidingSyncList`][super::SlidingSyncList].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(PartialEq))]
pub enum RoomListEntry {
    /// The list knows there is an entry but this entry has not been loaded yet,
    /// thus it's marked as empty.
    #[default]
    Empty,
    /// The list has loaded this entry in the past, but the entry is now out of
    /// range and may no longer be synced, thus it's marked as invalidated (to
    /// use the spec's term).
    Invalidated(OwnedRoomId),
    /// The list has loaded this entry, and it's up-to-date.
    Filled(OwnedRoomId),
}

impl RoomListEntry {
    /// Is this entry empty or invalidated?
    pub fn is_empty_or_invalidated(&self) -> bool {
        matches!(self, Self::Empty | Self::Invalidated(_))
    }

    /// Return the inner `room_id` if the entry' state is not empty.
    pub fn as_room_id(&self) -> Option<&RoomId> {
        match &self {
            Self::Empty => None,
            Self::Invalidated(room_id) | Self::Filled(room_id) => Some(room_id.as_ref()),
        }
    }

    /// Clone this entry, but freeze it, i.e. if the entry is empty, it remains
    /// empty, otherwise it is invalidated.
    pub(super) fn freeze_by_ref(&self) -> Self {
        match &self {
            Self::Empty => Self::Empty,
            Self::Invalidated(room_id) | Self::Filled(room_id) => {
                Self::Invalidated(room_id.clone())
            }
        }
    }
}

impl<'a> From<&'a RoomListEntry> for RoomListEntry {
    fn from(value: &'a RoomListEntry) -> Self {
        value.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use ruma::room_id;
    use serde_json::json;

    use super::*;

    macro_rules! assert_json_roundtrip {
        (from $type:ty: $rust_value:expr => $json_value:expr) => {
            let json = serde_json::to_value(&$rust_value).unwrap();
            assert_eq!(json, $json_value);

            let rust: $type = serde_json::from_value(json).unwrap();
            assert_eq!(rust, $rust_value);
        };
    }

    #[test]
    fn test_room_list_entry_is_empty_or_invalidated() {
        let room_id = room_id!("!foo:bar.org");

        assert!(RoomListEntry::Empty.is_empty_or_invalidated());
        assert!(RoomListEntry::Invalidated(room_id.to_owned()).is_empty_or_invalidated());
        assert!(RoomListEntry::Filled(room_id.to_owned()).is_empty_or_invalidated().not());
    }

    #[test]
    fn test_room_list_entry_as_room_id() {
        let room_id = room_id!("!foo:bar.org");

        assert_eq!(RoomListEntry::Empty.as_room_id(), None);
        assert_eq!(RoomListEntry::Invalidated(room_id.to_owned()).as_room_id(), Some(room_id));
        assert_eq!(RoomListEntry::Filled(room_id.to_owned()).as_room_id(), Some(room_id));
    }

    #[test]
    fn test_room_list_entry_freeze() {
        let room_id = room_id!("!foo:bar.org");

        assert_eq!(RoomListEntry::Empty.freeze_by_ref(), RoomListEntry::Empty);
        assert_eq!(
            RoomListEntry::Invalidated(room_id.to_owned()).freeze_by_ref(),
            RoomListEntry::Invalidated(room_id.to_owned())
        );
        assert_eq!(
            RoomListEntry::Filled(room_id.to_owned()).freeze_by_ref(),
            RoomListEntry::Invalidated(room_id.to_owned())
        );
    }

    #[test]
    fn test_room_list_entry_serialization() {
        let room_id = room_id!("!foo:bar.org");

        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Empty => json!("Empty"));
        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Invalidated(room_id.to_owned()) => json!({"Invalidated": "!foo:bar.org"}));
        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Filled(room_id.to_owned()) => json!({"Filled": "!foo:bar.org"}));
    }
}
