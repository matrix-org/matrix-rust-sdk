// Copyright 2024 Mauro Romito
// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::room_directory_search::RoomDirectorySearch as SdkRoomDirectorySearch;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::ServerName;
use tokio::sync::RwLock;

use crate::{error::ClientError, runtime::get_runtime_handle, task_handle::TaskHandle};

#[derive(uniffi::Enum)]
pub enum PublicRoomJoinRule {
    Public,
    Knock,
    Restricted,
    KnockRestricted,
    Invite,
}

impl TryFrom<ruma::room::JoinRuleKind> for PublicRoomJoinRule {
    type Error = String;

    fn try_from(value: ruma::room::JoinRuleKind) -> Result<Self, Self::Error> {
        match value {
            ruma::room::JoinRuleKind::Public => Ok(Self::Public),
            ruma::room::JoinRuleKind::Knock => Ok(Self::Knock),
            ruma::room::JoinRuleKind::Restricted => Ok(Self::Restricted),
            ruma::room::JoinRuleKind::KnockRestricted => Ok(Self::KnockRestricted),
            ruma::room::JoinRuleKind::Invite => Ok(Self::Invite),
            rule => Err(format!("unsupported join rule: {rule:?}")),
        }
    }
}

#[derive(uniffi::Record)]
pub struct RoomDescription {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub alias: Option<String>,
    pub avatar_url: Option<String>,
    pub join_rule: Option<PublicRoomJoinRule>,
    pub is_world_readable: bool,
    pub joined_members: u64,
}

impl From<matrix_sdk::room_directory_search::RoomDescription> for RoomDescription {
    fn from(value: matrix_sdk::room_directory_search::RoomDescription) -> Self {
        Self {
            room_id: value.room_id.to_string(),
            name: value.name,
            topic: value.topic,
            alias: value.alias.map(|alias| alias.to_string()),
            avatar_url: value.avatar_url.map(|url| url.to_string()),
            join_rule: value.join_rule.try_into().ok(),
            is_world_readable: value.is_world_readable,
            joined_members: value.joined_members,
        }
    }
}

/// A helper for performing room searches in the room directory.
/// The way this is intended to be used is:
///
/// 1. Register a callback using [`RoomDirectorySearch::results`].
/// 2. Start the room search with [`RoomDirectorySearch::search`].
/// 3. To get more results, use [`RoomDirectorySearch::next_page`].
#[derive(uniffi::Object)]
pub struct RoomDirectorySearch {
    pub(crate) inner: RwLock<SdkRoomDirectorySearch>,
}

impl RoomDirectorySearch {
    pub fn new(inner: SdkRoomDirectorySearch) -> Self {
        Self { inner: RwLock::new(inner) }
    }
}

#[matrix_sdk_ffi_macros::export]
impl RoomDirectorySearch {
    /// Asks the server for the next page of the current search.
    pub async fn next_page(&self) -> Result<(), ClientError> {
        let mut inner = self.inner.write().await;
        inner.next_page().await?;
        Ok(())
    }

    /// Starts a filtered search for the server.
    ///
    /// If the `filter` is not provided it will search for all the rooms.
    /// You can specify a `batch_size` to control the number of rooms to fetch
    /// per request.
    ///
    /// If the `via_server` is not provided it will search in the current
    /// homeserver by default.
    ///
    /// This method will clear the current search results and start a new one.
    pub async fn search(
        &self,
        filter: Option<String>,
        batch_size: u32,
        via_server_name: Option<String>,
    ) -> Result<(), ClientError> {
        let server = via_server_name.map(ServerName::parse).transpose()?;
        let mut inner = self.inner.write().await;
        inner.search(filter, batch_size, server).await?;
        Ok(())
    }

    /// Get the number of pages that have been loaded so far.
    pub async fn loaded_pages(&self) -> Result<u32, ClientError> {
        let inner = self.inner.read().await;
        Ok(inner.loaded_pages() as u32)
    }

    /// Get whether the search is at the last page.
    pub async fn is_at_last_page(&self) -> Result<bool, ClientError> {
        let inner = self.inner.read().await;
        Ok(inner.is_at_last_page())
    }

    /// Registers a callback to receive new search results when starting a
    /// search or getting new paginated results.
    pub async fn results(
        &self,
        listener: Box<dyn RoomDirectorySearchEntriesListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.read().await.results();

        listener.on_update(vec![RoomDirectorySearchEntryUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(|diff| diff.into()).collect());
            }
        })))
    }
}

#[derive(uniffi::Enum)]
pub enum RoomDirectorySearchEntryUpdate {
    Append { values: Vec<RoomDescription> },
    Clear,
    PushFront { value: RoomDescription },
    PushBack { value: RoomDescription },
    PopFront,
    PopBack,
    Insert { index: u32, value: RoomDescription },
    Set { index: u32, value: RoomDescription },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<RoomDescription> },
}

impl From<VectorDiff<matrix_sdk::room_directory_search::RoomDescription>>
    for RoomDirectorySearchEntryUpdate
{
    fn from(diff: VectorDiff<matrix_sdk::room_directory_search::RoomDescription>) -> Self {
        match diff {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(|v| v.into()).collect() }
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
                Self::Reset { values: values.into_iter().map(|v| v.into()).collect() }
            }
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomDirectorySearchEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, room_entries_update: Vec<RoomDirectorySearchEntryUpdate>);
}
