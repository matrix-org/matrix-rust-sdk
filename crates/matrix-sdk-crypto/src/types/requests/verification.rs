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

use ruma::TransactionId;

use super::{RoomMessageRequest, ToDeviceRequest};

/// An enum over the different outgoing verification based requests.
#[derive(Clone, Debug)]
pub enum OutgoingVerificationRequest {
    /// The to-device verification request variant.
    ToDevice(ToDeviceRequest),
    /// The in-room verification request variant.
    // `Box` the `RoomMessageRequest` to reduce the size of the enum.
    InRoom(Box<RoomMessageRequest>),
}

impl OutgoingVerificationRequest {
    /// Get the unique id of this request.
    pub fn request_id(&self) -> &TransactionId {
        match self {
            OutgoingVerificationRequest::ToDevice(t) => &t.txn_id,
            OutgoingVerificationRequest::InRoom(r) => &r.txn_id,
        }
    }
}

impl From<ToDeviceRequest> for OutgoingVerificationRequest {
    fn from(r: ToDeviceRequest) -> Self {
        OutgoingVerificationRequest::ToDevice(r)
    }
}

impl From<RoomMessageRequest> for OutgoingVerificationRequest {
    fn from(r: RoomMessageRequest) -> Self {
        OutgoingVerificationRequest::InRoom(Box::new(r))
    }
}
