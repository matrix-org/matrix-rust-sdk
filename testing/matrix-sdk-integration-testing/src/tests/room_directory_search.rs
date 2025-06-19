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

use std::time::Duration;

use anyhow::Result;
use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures::StreamExt;
use matrix_sdk::{
    room_directory_search::RoomDirectorySearch,
    ruma::api::client::room::{Visibility, create_room::v3::Request as CreateRoomRequest},
};
use rand::{Rng, thread_rng};
use stream_assert::assert_pending;
use tokio::time::sleep;
use tracing::warn;

use crate::helpers::TestClientBuilder;

#[tokio::test(flavor = "multi_thread")]
async fn test_room_directory_search_filter() -> Result<()> {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;
    let search_string = random_string(32);
    for index in 0..25 {
        let mut request: CreateRoomRequest = CreateRoomRequest::new();
        request.visibility = Visibility::Public;
        let name = format!("{search_string}_{index}");
        warn!("room name: {}", name);
        request.name = Some(name);
        alice.create_room(request).await?;
    }
    sleep(Duration::from_secs(1)).await;
    let mut room_directory_search = RoomDirectorySearch::new(alice);
    let (values, mut stream) = room_directory_search.results();
    assert!(values.is_empty());
    room_directory_search.search(Some(search_string), 10, None).await?;
    let results_batch: Vec<VectorDiff<matrix_sdk::room_directory_search::RoomDescription>> =
        stream.next().await.unwrap();
    assert_eq!(results_batch.len(), 1);
    assert_matches!(&results_batch[0], VectorDiff::Append { values } => { assert_eq!(values.len(), 10); });

    room_directory_search.next_page().await?;
    room_directory_search.next_page().await?;
    let results_batch = stream.next().await.unwrap();
    assert_eq!(results_batch.len(), 2);
    assert_matches!(&results_batch[0], VectorDiff::Append { values } => { assert_eq!(values.len(), 10); });
    assert_matches!(&results_batch[1], VectorDiff::Append { values } => { assert_eq!(values.len(), 5); });
    assert_pending!(stream);
    room_directory_search.next_page().await?;
    assert_pending!(stream);

    // This should reset the state completely
    room_directory_search.search(None, 25, None).await?;
    let results_batch = stream.next().await.unwrap();
    assert_matches!(&results_batch[0], VectorDiff::Clear);
    assert_matches!(&results_batch[1], VectorDiff::Append { values } => { assert_eq!(values.len(), 25); });
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
