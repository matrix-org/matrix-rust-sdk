// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Basic types and data structures to allow tracking of read receipts in rooms.
//!
//! These are defined in this crate because they're used in the `RoomInfo`,
//! which is also defined in this crate.
//!
//! All the fields are public by default, because they're mutated by other
//! crates upwards in the dependency tree (notably by the event cache in the
//! matrix-sdk crate).

use std::num::NonZeroUsize;

use matrix_sdk_common::ring_buffer::RingBuffer;
use ruma::OwnedEventId;
use serde::{Deserialize, Serialize};

/// The latest read receipt known for a room.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LatestReadReceipt {
    /// The id of the event the read receipt is referring to. (Not the read
    /// receipt event id.)
    pub event_id: OwnedEventId,
}

/// Public data about read receipts collected during processing of that room.
///
/// Remember that each time a field of `RoomReadReceipts` is updated in
/// `compute_unread_counts`, this function must return true!
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoomReadReceipts {
    /// Does the room have unread messages?
    pub num_unread: u64,

    /// Does the room have unread events that should notify?
    pub num_notifications: u64,

    /// Does the room have messages causing highlights for the users? (aka
    /// mentions)
    pub num_mentions: u64,

    /// The latest read receipt (main-threaded or unthreaded) known for the
    /// room.
    pub latest_active: Option<LatestReadReceipt>,

    /// Read receipts that haven't been matched to their event.
    ///
    /// This might mean that the read receipt is in the past further than we
    /// recall (i.e. before the first event we've ever cached), or in the
    /// future (i.e. the event is lagging behind because of federation).
    ///
    /// Note: this contains event ids of the event *targets* of the receipts,
    /// not the event ids of the receipt events themselves.
    #[serde(default = "new_nonempty_ring_buffer")]
    pub pending: RingBuffer<OwnedEventId>,
}

impl Default for RoomReadReceipts {
    fn default() -> Self {
        Self {
            num_unread: Default::default(),
            num_notifications: Default::default(),
            num_mentions: Default::default(),
            latest_active: Default::default(),
            pending: new_nonempty_ring_buffer(),
        }
    }
}

fn new_nonempty_ring_buffer() -> RingBuffer<OwnedEventId> {
    // 10 pending read receipts per room should be enough for everyone.
    // SAFETY: `unwrap` is safe because 10 is not zero.
    RingBuffer::new(NonZeroUsize::new(10).unwrap())
}
