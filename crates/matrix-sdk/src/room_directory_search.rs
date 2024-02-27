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

use eyeball_im::{ObservableVector, VectorDiff, VectorSubscriber};
use futures_core::Stream;
use ruma::{
    api::client::directory::get_public_rooms_filtered::v3::Request as PublicRoomsFilterRequest,
    directory::{Filter, PublicRoomJoinRule},
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId,
};

use crate::{Client, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomDescription {
    pub room_id: OwnedRoomId,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub alias: Option<OwnedRoomAliasId>,
    pub avatar_url: Option<OwnedMxcUri>,
    pub join_rule: PublicRoomJoinRule,
    pub is_world_readable: bool,
    pub joined_members: u64,
}

pub struct RoomDirectorySearch {
    batch_size: u32,
    filter: Option<String>,
    next_token: Option<String>,
    client: Client,
    results: ObservableVector<RoomDescription>,
    is_at_last_page: bool,
}

impl RoomDirectorySearch {
    pub fn new(client: Client) -> Self {
        Self {
            batch_size: 0,
            filter: None,
            next_token: None,
            client,
            results: ObservableVector::new(),
            is_at_last_page: false,
        }
    }

    pub async fn search(&mut self, filter: Option<String>, batch_size: u32) -> Result<()> {
        self.filter = filter;
        self.batch_size = batch_size;
        self.next_token = None;
        self.results.clear();
        self.is_at_last_page = false;
        self.next_page().await
    }

    pub async fn next_page(&mut self) -> Result<()> {
        if self.is_at_last_page {
            return Ok(());
        }
        let mut filter = Filter::new();
        filter.generic_search_term = self.filter.clone();

        let mut request = PublicRoomsFilterRequest::new();
        request.filter = filter;
        request.limit = Some(self.batch_size.into());
        request.since = self.next_token.clone();
        let response = self.client.public_rooms_filtered(request).await?;
        self.next_token = response.next_batch;
        if self.next_token.is_none() {
            self.is_at_last_page = true;
        }
        self.results.append(
            response
                .chunk
                .into_iter()
                .map(|room| RoomDescription {
                    room_id: room.room_id,
                    name: room.name,
                    topic: room.topic,
                    alias: room.canonical_alias,
                    avatar_url: room.avatar_url,
                    join_rule: room.join_rule,
                    is_world_readable: room.world_readable,
                    joined_members: room.num_joined_members.into(),
                })
                .collect(),
        );
        Ok(())
    }

    pub fn results(&self) -> impl Stream<Item = VectorDiff<RoomDescription>> {
        self.results.subscribe().into_stream()
    }

    pub fn loaded_pages(&self) -> usize {
        if self.batch_size == 0 {
            return 0;
        }
        self.results.len() / self.batch_size as usize
    }

    pub fn is_at_last_page(&self) -> bool {
        self.is_at_last_page
    }
}
