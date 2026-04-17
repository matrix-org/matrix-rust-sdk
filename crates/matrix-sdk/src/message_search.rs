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

use matrix_sdk_search::error::IndexError;
#[cfg(doc)]
use matrix_sdk_search::index::RoomIndex;
use ruma::OwnedEventId;

use crate::Room;

impl Room {
    /// Search this room's [`RoomIndex`] for query and return at most
    /// max_number_of_results results.
    pub async fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
    ) -> Result<Vec<OwnedEventId>, IndexError> {
        let mut search_index_guard = self.client.search_index().lock().await;
        search_index_guard.search(query, max_number_of_results, pagination_offset, self.room_id())
    }
}
