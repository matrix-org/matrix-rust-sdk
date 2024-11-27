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

//! Modules containing customized request types.

use std::sync::Arc;

use ruma::{OwnedTransactionId, TransactionId};

mod enums;
mod keys_backup;
mod keys_query;
mod room_message;
mod signing_keys;
mod to_device;
mod verification;

pub use enums::*;
pub use keys_backup::*;
pub use keys_query::*;
pub use room_message::*;
pub use signing_keys::*;
pub use to_device::*;
pub use verification::*;

/// Outgoing request type, holds the unique ID of the request and the actual
/// request.
#[derive(Debug, Clone)]
pub struct OutgoingRequest {
    /// The unique id of a request, needs to be passed when receiving a
    /// response.
    pub(crate) request_id: OwnedTransactionId,
    /// The underlying outgoing request.
    pub(crate) request: Arc<AnyOutgoingRequest>,
}

impl OutgoingRequest {
    /// Get the unique id of this request.
    pub fn request_id(&self) -> &TransactionId {
        &self.request_id
    }

    /// Get the underlying outgoing request.
    pub fn request(&self) -> &AnyOutgoingRequest {
        &self.request
    }
}
