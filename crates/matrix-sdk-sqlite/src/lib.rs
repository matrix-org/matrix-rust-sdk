// Copyright 2022 The Matrix.org Foundation C.I.C.
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
#![cfg_attr(
    not(any(feature = "state-store", feature = "crypto-store", feature = "event-cache")),
    allow(dead_code, unused_imports)
)]

#[cfg(feature = "crypto-store")]
mod crypto_store;
mod error;
#[cfg(feature = "event-cache")]
mod event_cache_store;
#[cfg(feature = "state-store")]
mod state_store;
mod utils;

#[cfg(feature = "crypto-store")]
pub use self::crypto_store::SqliteCryptoStore;
pub use self::error::OpenStoreError;
#[cfg(feature = "event-cache")]
pub use self::event_cache_store::SqliteEventCacheStore;
#[cfg(feature = "state-store")]
pub use self::state_store::SqliteStateStore;

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();
