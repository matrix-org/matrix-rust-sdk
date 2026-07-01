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

use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::{StreamExt as _, pin_mut};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::search_service::{
    MessageResult as UIMessageResult, PaginationState as SearchServicePaginationState,
    ResultType as UIResultType, SearchService as UISearchService,
};

use crate::{
    TaskHandle,
    client::Client,
    error::ClientError,
    runtime::get_runtime_handle,
    timeline::{ProfileDetails, TimelineItemContent},
    utils::Timestamp,
};

#[matrix_sdk_ffi_macros::export]
impl Client {
    /// Create a search service.
    ///
    /// The search service aggregates results of different kinds (currently only
    /// messages) into a single reactive, paginated list of typed
    /// [`SearchResult`]s. Call [`SearchService::set_query`] to start or update
    /// the search, then [`SearchService::paginate`] to load more results.
    pub fn search_service(&self) -> Arc<SearchService> {
        Arc::new(SearchService { inner: UISearchService::new((*self.inner).clone()) })
    }
}

/// A reactive, paginated search across all the user's data.
#[derive(uniffi::Object)]
pub struct SearchService {
    inner: UISearchService,
}

#[matrix_sdk_ffi_macros::export]
impl SearchService {
    /// Set (or update) the search query.
    /// Clears the current results, restarts pagination from scratch and loads
    /// the first page. Call [`Self::paginate`] to load any further pages.
    pub async fn set_query(&self, query: String) -> Result<(), ClientError> {
        self.inner.set_query(query).await.map_err(|err| ClientError::from(anyhow::Error::from(err)))
    }

    /// Load the next page of results if a page isn't already loading and the
    /// end hasn't been reached. Otherwise it no-ops.
    pub async fn paginate(&self) -> Result<(), ClientError> {
        self.inner.paginate().await.map_err(|err| ClientError::from(anyhow::Error::from(err)))
    }

    /// Returns the current pagination state.
    pub fn pagination_state(&self) -> SearchServicePaginationState {
        self.inner.pagination_state()
    }

    /// Subscribe to pagination state updates.
    pub fn subscribe_to_pagination_state_updates(
        &self,
        listener: Box<dyn SearchServicePaginationStateListener>,
    ) -> Arc<TaskHandle> {
        let pagination_state = self.inner.subscribe_to_pagination_state_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(pagination_state);

            while let Some(state) = pagination_state.next().await {
                listener.on_update(state);
            }
        })))
    }

    /// Subscribe to the search results.
    pub async fn subscribe_to_results(
        &self,
        listener: Box<dyn SearchServiceResultsListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_results().await;

        listener.on_update(vec![SearchServiceResultsUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SearchServicePaginationStateListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, pagination_state: SearchServicePaginationState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SearchServiceResultsListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, updates: Vec<SearchServiceResultsUpdate>);
}

/// A single search result, tagged by the kind of entity it represents.
#[derive(uniffi::Enum)]
pub enum SearchServiceResult {
    /// A message (room timeline event) matching the query.
    Message { room_id: String, result: MessageSearchResult },
}

impl From<UIResultType> for SearchServiceResult {
    fn from(result: UIResultType) -> Self {
        match result {
            UIResultType::Message(message) => {
                let room_id = message.room_id.to_string();
                Self::Message { room_id, result: message.into() }
            }
        }
    }
}

/// A message matching a search query, with its content and sender resolved.
#[derive(Clone, uniffi::Record)]
pub struct MessageSearchResult {
    event_id: String,
    sender: String,
    sender_profile: ProfileDetails,
    content: TimelineItemContent,
    timestamp: Timestamp,
}

impl From<UIMessageResult> for MessageSearchResult {
    fn from(result: UIMessageResult) -> Self {
        Self {
            event_id: result.event_id.to_string(),
            sender: result.sender.to_string(),
            sender_profile: ProfileDetails::from(result.sender_profile),
            content: TimelineItemContent::from(result.content),
            timestamp: result.timestamp.into(),
        }
    }
}

#[derive(uniffi::Enum)]
pub enum SearchServiceResultsUpdate {
    Append { values: Vec<SearchServiceResult> },
    Clear,
    PushFront { value: SearchServiceResult },
    PushBack { value: SearchServiceResult },
    PopFront,
    PopBack,
    Insert { index: u32, value: SearchServiceResult },
    Set { index: u32, value: SearchServiceResult },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<SearchServiceResult> },
}

impl From<VectorDiff<UIResultType>> for SearchServiceResultsUpdate {
    fn from(diff: VectorDiff<UIResultType>) -> Self {
        match diff {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(Into::into).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => Self::PushFront { value: value.into() },
            VectorDiff::PushBack { value } => Self::PushBack { value: value.into() },
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: index as u32, value: value.into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: index as u32, value: value.into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: index as u32 },
            VectorDiff::Truncate { length } => Self::Truncate { length: length as u32 },
            VectorDiff::Reset { values } => {
                Self::Reset { values: values.into_iter().map(Into::into).collect() }
            }
        }
    }
}
