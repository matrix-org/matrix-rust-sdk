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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::future::ready;

use eyeball_im::{Vector, VectorDiff};
use futures_util::Stream;
use matrix_sdk::{RoomListEntry, SlidingSync, SlidingSyncList};

use super::Error;

#[derive(Debug)]
pub struct RoomList {
    sliding_sync_list: SlidingSyncList,
}

impl RoomList {
    pub(super) async fn new(
        sliding_sync: &SlidingSync,
        sliding_sync_list_name: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            sliding_sync_list: sliding_sync
                .on_list(sliding_sync_list_name, |list| ready(list.clone()))
                .await
                .ok_or_else(|| Error::UnknownList(sliding_sync_list_name.to_owned()))?,
        })
    }

    /// Get all previous room list entries, in addition to a [`Stream`] to room
    /// list entry's updates.
    pub fn entries(
        &self,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>) {
        self.sliding_sync_list.room_list_stream()
    }

    /// Similar to [`Self::entries`] except that it's possible to provide a
    /// filter that will filter out room list entries.
    pub fn entries_filtered<F>(
        &self,
        filter: F,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>)
    where
        F: Fn(&RoomListEntry) -> bool + Send + Sync + 'static,
    {
        self.sliding_sync_list.room_list_filtered_stream(filter)
    }
}
