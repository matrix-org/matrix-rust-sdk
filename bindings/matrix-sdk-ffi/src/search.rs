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

use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk_ui::search::RoomSearch;
use ruma::OwnedUserId;
use tokio::sync::Mutex;

use crate::{
    error::ClientError,
    room::Room,
    timeline::{ProfileDetails, TimelineItemContent},
    utils::Timestamp,
};

#[matrix_sdk_ffi_macros::export]
impl Room {
    pub fn search(&self, query: String) -> RoomSearchIterator {
        RoomSearchIterator {
            sdk_room: self.inner.clone(),
            inner: Mutex::new(RoomSearch::new(self.inner.clone(), query)),
        }
    }
}

#[derive(uniffi::Object)]
pub struct RoomSearchIterator {
    sdk_room: matrix_sdk::room::Room,
    inner: Mutex<RoomSearch>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomSearchIterator {
    /// Return a list of event ids for the next batch of search results, or
    /// `None` if there are no more results.
    pub async fn next(&self) -> Option<Vec<String>> {
        match self.inner.lock().await.next().await {
            Ok(Some(event_ids)) => Some(event_ids.into_iter().map(|id| id.to_string()).collect()),
            Ok(None) => None,
            Err(e) => {
                eprintln!("Error during search: {e}");
                None
            }
        }
    }

    /// Return a list of events for the next batch of search results, or `None`
    /// if there are no more results.
    pub async fn next_events(&self) -> Result<Option<Vec<RoomSearchResult>>, ClientError> {
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
        // TODO: i did make an helper for this, on some branch on my machine
        let sender = event.raw().get_field::<OwnedUserId>("sender").ok().flatten()?;

        let event_id = event.event_id().unwrap().to_string();
        let timestamp =
            event.timestamp().unwrap_or_else(ruma::MilliSecondsSinceUnixEpoch::now).into();

        let (content, profile) =
            matrix_sdk_ui::timeline::TimelineItemContent::from_raw_event(room, event).await?;

        Some(Self {
            event_id,
            sender: sender.to_string(),
            sender_profile: ProfileDetails::from(profile),
            content: TimelineItemContent::from(content),
            timestamp,
        })
    }
}
