// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use ruma::{OwnedRoomId, OwnedTransactionId, events::AnyMessageLikeEventContent};

/// Customized owned request type for sending out room messages.
#[derive(Clone, Debug)]
pub struct RoomMessageRequest {
    /// The room to send the event to.
    pub room_id: OwnedRoomId,

    /// The transaction ID for this event.
    ///
    /// Clients should generate an ID unique across requests with the
    /// same access token; it will be used by the server to ensure
    /// idempotency of requests.
    pub txn_id: OwnedTransactionId,

    /// The event content to send.
    pub content: Box<AnyMessageLikeEventContent>,
}
