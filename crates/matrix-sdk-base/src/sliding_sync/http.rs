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

//! HTTP types for MSC4186 or MSC3585.
//!
//! This module provides unified namings for types from MSC3575 and
//! MSC4186.

/// HTTP types from MSC3575, renamed to match the MSC4186 namings.
pub mod msc3575 {
    use ruma::api::client::sync::sync_events::v4;
    pub use v4::{Request, Response};

    /// HTTP types related to a `Request`.
    pub mod request {
        pub use super::v4::{
            AccountDataConfig as AccountData, ExtensionsConfig as Extensions,
            ReceiptsConfig as Receipts, RoomDetailsConfig as RoomDetails, RoomSubscription,
            SyncRequestList as List, SyncRequestListFilters as ListFilters,
            ToDeviceConfig as ToDevice, TypingConfig as Typing,
        };
    }

    /// HTTP types related to a `Response`.
    pub mod response {
        pub use super::v4::{
            AccountData, Extensions, Receipts, SlidingSyncRoom as Room,
            SlidingSyncRoomHero as RoomHero, SyncList as List, ToDevice, Typing,
        };
    }
}

/// HTTP types from MSC4186.
pub mod msc4186 {
    pub use ruma::api::client::sync::sync_events::v5::*;
}

pub use msc4186::*;
