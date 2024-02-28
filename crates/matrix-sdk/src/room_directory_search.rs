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

//! Types for searching the public room directory.

use eyeball_im::{ObservableVector, VectorDiff};
use futures_core::Stream;
use imbl::Vector;
use ruma::{
    api::client::directory::get_public_rooms_filtered::v3::Request as PublicRoomsFilterRequest,
    directory::{Filter, PublicRoomJoinRule},
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId,
};

use crate::{Client, Result};

/// This struct represents a single result of a room directory search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomDescription {
    /// The room's ID.
    pub room_id: OwnedRoomId,
    /// The name of the room, if any.
    pub name: Option<String>,
    /// The topic of the room, if any.
    pub topic: Option<String>,
    /// The canonical alias of the room, if any.
    pub alias: Option<OwnedRoomAliasId>,
    /// The room's avatar URL, if any.
    pub avatar_url: Option<OwnedMxcUri>,
    /// The room's join rule.
    pub join_rule: PublicRoomJoinRule,
    /// Whether can be previewed
    pub is_world_readable: bool,
    /// The number of members that have joined the room.
    pub joined_members: u64,
}

impl From<ruma::directory::PublicRoomsChunk> for RoomDescription {
    fn from(value: ruma::directory::PublicRoomsChunk) -> Self {
        Self {
            room_id: value.room_id,
            name: value.name,
            topic: value.topic,
            alias: value.canonical_alias,
            avatar_url: value.avatar_url,
            join_rule: value.join_rule,
            is_world_readable: value.world_readable,
            joined_members: value.num_joined_members.into(),
        }
    }
}

#[derive(Debug)]
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

    /// Starts a filtered search for the server
    /// If the `filter` is not provided it will search for all the rooms
    /// You can specify a `batch_size`` to control the number of rooms to fetch
    /// per request
    ///
    /// This method will clear the current search results and start a new one
    /// Should never be used concurrently with another `next_page` or a
    /// `search`.
    pub async fn search(&mut self, filter: Option<String>, batch_size: u32) -> Result<()> {
        self.filter = filter;
        self.batch_size = batch_size;
        self.next_token = None;
        self.results.clear();
        self.is_at_last_page = false;
        self.next_page().await
    }

    /// Asks the server for the next page of the current search
    /// Should never be used concurrently with another `next_page` or a
    /// `search`.
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
        self.results.append(response.chunk.into_iter().map(Into::into).collect());
        Ok(())
    }

    /// Get the initial value of the current stored room descriptions in the
    /// search, and a stream of updates for them.
    pub fn results(
        &self,
    ) -> (Vector<RoomDescription>, impl Stream<Item = Vec<VectorDiff<RoomDescription>>>) {
        self.results.subscribe().into_values_and_batched_stream()
    }

    /// Get the number of pages that have been loaded so far
    pub fn loaded_pages(&self) -> usize {
        if self.batch_size == 0 {
            return 0;
        }
        self.results.len() / self.batch_size as usize
    }

    /// Get whether the search is at the last page
    pub fn is_at_last_page(&self) -> bool {
        self.is_at_last_page
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{directory::Filter, serde::Raw, RoomAliasId, RoomId};
    use serde_json::Value as JsonValue;
    use wiremock::{
        matchers::{method, path_regex},
        Match, Mock, MockServer, Request, ResponseTemplate,
    };

    use crate::{
        room_directory_search::{RoomDescription, RoomDirectorySearch},
        test_utils::logged_in_client,
        Client,
    };

    struct RoomDirectorySearchMatcher {
        next_token: Option<String>,
        filter_term: Option<String>,
        limit: u32,
    }

    impl Match for RoomDirectorySearchMatcher {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = request.body_json::<Raw<JsonValue>>() else {
                return false;
            };

            // The body's `since` field is set equal to the matcher's next_token.
            if !body.get_field::<String>("since").is_ok_and(|s| s == self.next_token) {
                return false;
            }

            if !body.get_field::<u32>("limit").is_ok_and(|s| s == Some(self.limit)) {
                return false;
            }

            // The body's `filter` field has `generic_search_term` equal to the matcher's
            // next_token.
            if !body.get_field::<Filter>("filter").is_ok_and(|s| {
                if self.filter_term.is_none() {
                    s.is_none() || s.is_some_and(|s| s.generic_search_term.is_none())
                } else {
                    s.is_some_and(|s| s.generic_search_term == self.filter_term)
                }
            }) {
                return false;
            }

            method("POST").matches(request)
                && path_regex("/_matrix/client/../publicRooms").matches(request)
        }
    }

    fn get_first_page_description() -> RoomDescription {
        RoomDescription {
            room_id: RoomId::parse("!ol19s:bleecker.street").unwrap(),
            name: Some("CHEESE".into()),
            topic: Some("Tasty tasty cheese".into()),
            alias: None,
            avatar_url: Some("mxc://bleeker.street/CHEDDARandBRIE".into()),
            join_rule: ruma::directory::PublicRoomJoinRule::Public,
            is_world_readable: true,
            joined_members: 37,
        }
    }

    fn get_second_page_description() -> RoomDescription {
        RoomDescription {
            room_id: RoomId::parse("!ca18r:bleecker.street").unwrap(),
            name: Some("PEAR".into()),
            topic: Some("Tasty tasty pear".into()),
            alias: RoomAliasId::parse("#murrays:pear.bar").ok(),
            avatar_url: Some("mxc://bleeker.street/pear".into()),
            join_rule: ruma::directory::PublicRoomJoinRule::Knock,
            is_world_readable: false,
            joined_members: 20,
        }
    }

    async fn new_server_and_client() -> (MockServer, Client) {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        (server, client)
    }

    #[async_test]
    async fn search_success() {
        let (server, client) = new_server_and_client().await;

        let mut room_directory_search = RoomDirectorySearch::new(client);
        Mock::given(RoomDirectorySearchMatcher { next_token: None, filter_term: None, limit: 1 })
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
            .mount(&server)
            .await;

        room_directory_search.search(None, 1).await.unwrap();
        let (results, _) = room_directory_search.results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], get_first_page_description());
        assert!(!room_directory_search.is_at_last_page);
        assert_eq!(room_directory_search.loaded_pages(), 1);
    }

    #[async_test]
    async fn search_success_paginated() {
        let (server, client) = new_server_and_client().await;

        let mut room_directory_search = RoomDirectorySearch::new(client);
        Mock::given(RoomDirectorySearchMatcher { next_token: None, filter_term: None, limit: 1 })
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
            .mount(&server)
            .await;

        room_directory_search.search(None, 1).await.unwrap();

        Mock::given(RoomDirectorySearchMatcher {
            next_token: Some("p190q".into()),
            filter_term: None,
            limit: 1,
        })
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS_FINAL_PAGE),
        )
        .mount(&server)
        .await;

        room_directory_search.next_page().await.unwrap();

        let (results, _) = room_directory_search.results();
        assert_eq!(
            results,
            vec![get_first_page_description(), get_second_page_description()].into()
        );
        assert!(room_directory_search.is_at_last_page);
        assert_eq!(room_directory_search.loaded_pages(), 2);
    }

    #[async_test]
    async fn search_fails() {
        let (server, client) = new_server_and_client().await;

        let mut room_directory_search = RoomDirectorySearch::new(client);
        Mock::given(RoomDirectorySearchMatcher { next_token: None, filter_term: None, limit: 1 })
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        room_directory_search.search(None, 1).await.unwrap_err();
        let (results, _) = room_directory_search.results();
        assert_eq!(results.len(), 0);
        assert!(!room_directory_search.is_at_last_page);
        assert_eq!(room_directory_search.loaded_pages(), 0);
    }

    #[async_test]
    async fn search_fails_when_paginating() {
        let (server, client) = new_server_and_client().await;

        let mut room_directory_search = RoomDirectorySearch::new(client);
        Mock::given(RoomDirectorySearchMatcher { next_token: None, filter_term: None, limit: 1 })
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
            .mount(&server)
            .await;

        room_directory_search.search(None, 1).await.unwrap();

        Mock::given(RoomDirectorySearchMatcher {
            next_token: Some("p190q".into()),
            filter_term: None,
            limit: 1,
        })
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

        room_directory_search.next_page().await.unwrap_err();

        let (results, _) = room_directory_search.results();
        assert_eq!(results, vec![get_first_page_description()].into());
        assert!(!room_directory_search.is_at_last_page);
        assert_eq!(room_directory_search.loaded_pages(), 1);
    }

    #[async_test]
    async fn search_success_paginated_with_filter() {
        let (server, client) = new_server_and_client().await;

        let mut room_directory_search = RoomDirectorySearch::new(client);
        Mock::given(RoomDirectorySearchMatcher {
            next_token: None,
            filter_term: Some("bleecker.street".into()),
            limit: 1,
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
        .mount(&server)
        .await;

        room_directory_search.search(Some("bleecker.street".into()), 1).await.unwrap();

        Mock::given(RoomDirectorySearchMatcher {
            next_token: Some("p190q".into()),
            filter_term: Some("bleecker.street".into()),
            limit: 1,
        })
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS_FINAL_PAGE),
        )
        .mount(&server)
        .await;

        room_directory_search.next_page().await.unwrap();

        let (results, _) = room_directory_search.results();
        assert_eq!(
            results,
            vec![get_first_page_description(), get_second_page_description()].into()
        );
        assert!(room_directory_search.is_at_last_page);
        assert_eq!(room_directory_search.loaded_pages(), 2);
    }
}
