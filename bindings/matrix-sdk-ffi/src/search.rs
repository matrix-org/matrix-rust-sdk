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
// See the License for that specific language governing permissions and
// limitations under the License.

use matrix_sdk::{
    deserialized_responses::TimelineEvent,
    message_search::{
        GlobalSearchIterator as SdkGlobalSearchIterator,
        RoomSearchIterator as SdkRoomSearchIterator, SearchError as SdkSearchError,
    },
};
use matrix_sdk_ui::timeline::TimelineDetails;
use tokio::sync::Mutex;

use crate::{
    client::Client,
    error::ClientError,
    room::Room,
    timeline::{ProfileDetails, TimelineItemContent},
    utils::Timestamp,
};

#[derive(uniffi::Error, thiserror::Error, Debug)]
pub enum SearchError {
    #[error("Failed to search through the index: {0}")]
    IndexError(String),
    #[error("Failed to load event content for search result: {0}")]
    EventLoadError(String),
}

impl From<SdkSearchError> for SearchError {
    fn from(err: SdkSearchError) -> Self {
        match err {
            SdkSearchError::IndexError(err) => SearchError::IndexError(err.to_string()),
            SdkSearchError::EventLoadError(err) => SearchError::EventLoadError(err.to_string()),
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Search for messages in this room matching the given query, returning an
    /// iterator over the results that yields `num_results_per_batch` results at
    /// a time.
    pub fn search_messages(&self, query: String, num_results_per_batch: u32) -> RoomSearchIterator {
        RoomSearchIterator {
            sdk_room: self.inner.clone(),
            inner: Mutex::new(self.inner.search_messages(query, num_results_per_batch as usize)),
        }
    }
}

#[derive(uniffi::Object)]
pub struct RoomSearchIterator {
    sdk_room: matrix_sdk::room::Room,
    inner: Mutex<SdkRoomSearchIterator>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomSearchIterator {
    /// Return a list of events for the next batch of search results, or `None`
    /// if there are no more results.
    pub async fn next_events(&self) -> Result<Option<Vec<RoomSearchResult>>, SearchError> {
        let Some(events) = self.inner.lock().await.next_events().await? else {
            return Ok(None);
        };

        let mut results = Vec::with_capacity(events.len());

        for event in events {
            if let Some(result) = RoomSearchResult::from(&self.sdk_room, event).await {
                results.push(result);
            }
        }

        results.shrink_to_fit();

        Ok(Some(results))
    }
}

#[derive(Clone, uniffi::Record)]
pub struct RoomSearchResult {
    event_id: String,
    sender: String,
    sender_profile: ProfileDetails,
    content: TimelineItemContent,
    timestamp: Timestamp,
}

impl RoomSearchResult {
    async fn from(room: &matrix_sdk::room::Room, event: TimelineEvent) -> Option<Self> {
        let sender = event.sender()?;

        let event_id = event.event_id().unwrap().to_string();
        let timestamp =
            event.timestamp().unwrap_or_else(ruma::MilliSecondsSinceUnixEpoch::now).into();

        let content = matrix_sdk_ui::timeline::TimelineItemContent::from_event(room, event).await?;
        let profile = TimelineDetails::from_initial_value(
            matrix_sdk_ui::timeline::Profile::load(room, &sender).await,
        );

        Some(Self {
            event_id,
            sender: sender.to_string(),
            sender_profile: ProfileDetails::from(profile),
            content: TimelineItemContent::from(content),
            timestamp,
        })
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum SearchRoomFilter {
    /// All the joined rooms (= DMs + non-DMs).
    Rooms,
    /// Only joined DM rooms.
    Dms,
    /// Only joined non-DM (group) rooms.
    NonDms,
}

#[matrix_sdk_ffi_macros::export]
impl Client {
    /// Search across all all rooms for the given query, returning an iterator
    /// over the results.
    pub async fn search_messages(
        &self,
        query: String,
        filter: SearchRoomFilter,
        num_results_per_batch: u32,
    ) -> Result<GlobalSearchIterator, ClientError> {
        let sdk_client = (*self.inner).clone();
        let mut search = sdk_client.search_messages(query, num_results_per_batch as usize);

        match filter {
            SearchRoomFilter::Rooms => {}
            SearchRoomFilter::Dms => search = search.only_dm_rooms().await?,
            SearchRoomFilter::NonDms => search = search.no_dms().await?,
        }

        Ok(GlobalSearchIterator { sdk_client, inner: Mutex::new(search.build()) })
    }
}

#[derive(uniffi::Record)]
pub struct GlobalSearchResult {
    room_id: String,
    result: RoomSearchResult,
}

#[derive(uniffi::Object)]
pub struct GlobalSearchIterator {
    sdk_client: matrix_sdk::Client,
    inner: Mutex<SdkGlobalSearchIterator>,
}

#[matrix_sdk_ffi_macros::export]
impl GlobalSearchIterator {
    /// Return a list of events for the next batch of search results, or `None`
    /// if there are no more results.
    pub async fn next_events(&self) -> Result<Option<Vec<GlobalSearchResult>>, SearchError> {
        let Some(events) = self.inner.lock().await.next_events().await? else {
            return Ok(None);
        };

        let mut results = Vec::with_capacity(events.len());

        for (room_id, event) in events {
            let Some(room) = self.sdk_client.get_room(&room_id) else {
                continue;
            };
            if let Some(result) = RoomSearchResult::from(&room, event).await {
                results.push(GlobalSearchResult { room_id: room_id.to_string(), result });
            }
        }

        results.shrink_to_fit();

        Ok(Some(results))
    }
}
