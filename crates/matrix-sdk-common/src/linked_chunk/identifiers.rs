// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::fmt;

use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};

/// An identifier for a linked chunk; borrowed variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkedChunkId<'a> {
    /// A room's unthreaded timeline.
    Room(&'a RoomId),
    /// A room's thread.
    Thread(&'a RoomId, &'a EventId),
    /// A room's list of pinned events.
    PinnedEvents(&'a RoomId),
    /// An event-focused timeline (e.g., for permalinks).
    EventFocused(&'a RoomId, &'a EventId),
}

impl fmt::Display for LinkedChunkId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Room(room_id) => write!(f, "{room_id}"),
            Self::Thread(room_id, thread_root) => {
                write!(f, "{room_id}:thread:{thread_root}")
            }
            Self::PinnedEvents(room_id) => {
                write!(f, "{room_id}:pinned")
            }
            Self::EventFocused(room_id, event_id) => {
                write!(f, "{room_id}:event_focused:{event_id}")
            }
        }
    }
}

impl LinkedChunkId<'_> {
    pub fn storage_key(&self) -> impl '_ + AsRef<[u8]> {
        match self {
            LinkedChunkId::Room(room_id) => room_id.to_string(),
            LinkedChunkId::Thread(room_id, event_id) => format!("t:{room_id}:{event_id}"),
            LinkedChunkId::PinnedEvents(room_id) => format!("pinned:{room_id}"),
            LinkedChunkId::EventFocused(room_id, event_id) => {
                format!("event_focused:{room_id}:{event_id}")
            }
        }
    }

    pub fn to_owned(&self) -> OwnedLinkedChunkId {
        match self {
            LinkedChunkId::Room(room_id) => OwnedLinkedChunkId::Room((*room_id).to_owned()),
            LinkedChunkId::Thread(room_id, event_id) => {
                OwnedLinkedChunkId::Thread((*room_id).to_owned(), (*event_id).to_owned())
            }
            LinkedChunkId::PinnedEvents(room_id) => {
                OwnedLinkedChunkId::PinnedEvents((*room_id).to_owned())
            }
            LinkedChunkId::EventFocused(room_id, event_id) => {
                OwnedLinkedChunkId::EventFocused((*room_id).to_owned(), (*event_id).to_owned())
            }
        }
    }
}

impl<'a> From<&'a OwnedLinkedChunkId> for LinkedChunkId<'a> {
    fn from(value: &'a OwnedLinkedChunkId) -> Self {
        value.as_ref()
    }
}

impl PartialEq<&OwnedLinkedChunkId> for LinkedChunkId<'_> {
    fn eq(&self, other: &&OwnedLinkedChunkId) -> bool {
        match (self, other) {
            (LinkedChunkId::Room(a), OwnedLinkedChunkId::Room(b)) => *a == b,
            (LinkedChunkId::PinnedEvents(a), OwnedLinkedChunkId::PinnedEvents(b)) => *a == b,
            (LinkedChunkId::Thread(r, ev), OwnedLinkedChunkId::Thread(r2, ev2)) => {
                r == r2 && ev == ev2
            }
            (LinkedChunkId::EventFocused(r, ev), OwnedLinkedChunkId::EventFocused(r2, ev2)) => {
                r == r2 && ev == ev2
            }
            _ => false,
        }
    }
}

impl PartialEq<LinkedChunkId<'_>> for OwnedLinkedChunkId {
    fn eq(&self, other: &LinkedChunkId<'_>) -> bool {
        other.eq(&self)
    }
}

/// An identifier for a linked chunk; owned variant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnedLinkedChunkId {
    Room(OwnedRoomId),
    Thread(OwnedRoomId, OwnedEventId),
    PinnedEvents(OwnedRoomId),
    EventFocused(OwnedRoomId, OwnedEventId),
}

impl fmt::Display for OwnedLinkedChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl OwnedLinkedChunkId {
    pub fn as_ref(&self) -> LinkedChunkId<'_> {
        match self {
            OwnedLinkedChunkId::Room(room_id) => LinkedChunkId::Room(room_id.as_ref()),
            OwnedLinkedChunkId::Thread(room_id, event_id) => {
                LinkedChunkId::Thread(room_id.as_ref(), event_id.as_ref())
            }
            OwnedLinkedChunkId::PinnedEvents(room_id) => {
                LinkedChunkId::PinnedEvents(room_id.as_ref())
            }
            OwnedLinkedChunkId::EventFocused(room_id, event_id) => {
                LinkedChunkId::EventFocused(room_id.as_ref(), event_id.as_ref())
            }
        }
    }

    pub fn room_id(&self) -> &RoomId {
        match self {
            OwnedLinkedChunkId::Room(room_id)
            | OwnedLinkedChunkId::Thread(room_id, ..)
            | OwnedLinkedChunkId::PinnedEvents(room_id, ..)
            | OwnedLinkedChunkId::EventFocused(room_id, ..) => room_id,
        }
    }
}

impl From<LinkedChunkId<'_>> for OwnedLinkedChunkId {
    fn from(value: LinkedChunkId<'_>) -> Self {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use ruma::{event_id, room_id};

    use super::LinkedChunkId;

    #[test]
    fn test_display() {
        assert_eq!(LinkedChunkId::Room(room_id!("!r")).to_string(), "!r");
        assert_eq!(
            LinkedChunkId::Thread(room_id!("!r"), event_id!("$e")).to_string(),
            "!r:thread:$e"
        );
        assert_eq!(LinkedChunkId::PinnedEvents(room_id!("!r")).to_string(), "!r:pinned");
        assert_eq!(
            LinkedChunkId::EventFocused(room_id!("!r"), event_id!("$e")).to_string(),
            "!r:event_focused:$e"
        );
    }

    #[test]
    fn test_storage_key() {
        assert_eq!(LinkedChunkId::Room(room_id!("!r")).storage_key().as_ref(), "!r".as_bytes());
        assert_eq!(
            LinkedChunkId::Thread(room_id!("!r"), event_id!("$e")).storage_key().as_ref(),
            "t:!r:$e".as_bytes()
        );
        assert_eq!(
            LinkedChunkId::PinnedEvents(room_id!("!r")).storage_key().as_ref(),
            "pinned:!r".as_bytes()
        );
        assert_eq!(
            LinkedChunkId::EventFocused(room_id!("!r"), event_id!("$e")).storage_key().as_ref(),
            "event_focused:!r:$e".as_bytes()
        );
    }

    #[test]
    fn test_partial_eq() {
        // Room.
        assert!(LinkedChunkId::Room(room_id!("!r0")) == LinkedChunkId::Room(room_id!("!r0")));
        assert!(LinkedChunkId::Room(room_id!("!r0")) != LinkedChunkId::Room(room_id!("!r1")));

        // Thread.
        assert!(
            LinkedChunkId::Thread(room_id!("!r0"), event_id!("$e0"))
                == LinkedChunkId::Thread(room_id!("!r0"), event_id!("$e0"))
        );
        assert!(
            LinkedChunkId::Thread(room_id!("!r0"), event_id!("$e0"))
                != LinkedChunkId::Thread(room_id!("!r1"), event_id!("$e0"))
        );
        assert!(
            LinkedChunkId::Thread(room_id!("!r0"), event_id!("$e0"))
                != LinkedChunkId::Thread(room_id!("!r0"), event_id!("$e1"))
        );

        // PinnedEvents.
        assert!(
            LinkedChunkId::PinnedEvents(room_id!("!r0"))
                == LinkedChunkId::PinnedEvents(room_id!("!r0"))
        );
        assert!(
            LinkedChunkId::PinnedEvents(room_id!("!r0"))
                != LinkedChunkId::PinnedEvents(room_id!("!r1"))
        );

        // EventFocused.
        assert!(
            LinkedChunkId::EventFocused(room_id!("!r0"), event_id!("$e0"))
                == LinkedChunkId::EventFocused(room_id!("!r0"), event_id!("$e0"))
        );
        assert!(
            LinkedChunkId::EventFocused(room_id!("!r0"), event_id!("$e0"))
                != LinkedChunkId::EventFocused(room_id!("!r1"), event_id!("$e0"))
        );
        assert!(
            LinkedChunkId::EventFocused(room_id!("!r0"), event_id!("$e0"))
                != LinkedChunkId::EventFocused(room_id!("!r0"), event_id!("$e1"))
        );
    }
}
