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

use eyeball_im::ObservableVector;
use futures_core::Stream;
use ruma::OwnedRoomId;

use crate::Client;

#[derive(Clone)]
struct RoomDescription {
    room_id: OwnedRoomId,
}
struct RoomDirectorySync {
    next_token: Option<String>,
    client: Client,
    results: ObservableVector<RoomDescription>,
}

impl RoomDirectorySync {
    async fn sync(&mut self) {
        // TODO: we may want to have the limit be a configurable parameter of the sync?
        // TODO: same for the server?
        loop {
            let response = self.client.public_rooms(None, self.next_token.as_deref(), None).await;
            if let Ok(response) = response {
                self.next_token = response.next_batch.clone();
                self.results.append(
                    response
                        .chunk
                        .into_iter()
                        .map(|room| RoomDescription { room_id: room.room_id })
                        .collect(),
                );
                if self.next_token.is_none() {
                    break;
                }
            }
        }
    }
}
