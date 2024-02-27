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

use anyhow::Result;
use eyeball_im::VectorDiff;
use futures::{FutureExt, StreamExt};
use matrix_sdk::{
    room_directory_search::RoomDirectorySearch,
    ruma::api::client::room::{create_room::v3::Request as CreateRoomRequest, Visibility},
};
use rand::{thread_rng, Rng};
use stream_assert::{assert_next_eq, assert_pending};
use tracing::warn;

use crate::helpers::TestClientBuilder;

#[tokio::test(flavor = "multi_thread")]
async fn test_room_directory_search_filter() -> Result<()> {
    let alice = TestClientBuilder::new("alice".to_owned()).use_sqlite().build().await?;
    let search_string = random_string(32);
    for index in 0..25 {
        let mut request: CreateRoomRequest = CreateRoomRequest::new();
        request.visibility = Visibility::Public;
        let name = format!("{}_{}", search_string, index);
        warn!("room name: {}", name);
        request.name = Some(name);
        alice.create_room(request).await?;
    }
    let mut room_directory_search = RoomDirectorySearch::new(alice);
    let mut stream = room_directory_search.results();
    room_directory_search.search(Some(search_string), 10).await?;
    assert_next_eq!(stream, VectorDiff::Clear);

    for _ in 0..2 {
        if let VectorDiff::Append { values } = stream.next().now_or_never().unwrap().unwrap() {
            warn!("Values: {:?}", values);
            assert_eq!(values.len(), 10);
        } else {
            panic!("Expected a Vector::Append");
        }
        assert_pending!(stream);
        room_directory_search.next_page().await?;
    }

    if let VectorDiff::Append { values } = stream.next().now_or_never().unwrap().unwrap() {
        warn!("Values: {:?}", values);
        assert_eq!(values.len(), 5);
    } else {
        panic!("Expected a Vector::Append");
    }
    assert_pending!(stream);
    room_directory_search.next_page().await?;
    assert_pending!(stream);

    // This should reset the state completely
    room_directory_search.search(None, 25).await?;
    assert_next_eq!(stream, VectorDiff::Clear);
    if let VectorDiff::Append { values } = stream.next().now_or_never().unwrap().unwrap() {
        warn!("Values: {:?}", values);
        assert_eq!(values.len(), 25);
    } else {
        panic!("Expected a Vector::Append");
    }
    assert_pending!(stream);
    Ok(())
}

fn random_string(length: usize) -> String {
    thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}
