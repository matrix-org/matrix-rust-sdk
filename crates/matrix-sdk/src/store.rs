// Copyright 2022 Kévin Commaille
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

//! Functions and types to initialize a store.
//!
//! The re-exports present here depend on the store-related features that are
//! enabled.

#[cfg(feature = "indexeddb_stores")]
pub use matrix_sdk_indexeddb::*;
#[cfg(any(feature = "sled_state_store", feature = "sled_cryptostore"))]
pub use matrix_sdk_sled::*;
