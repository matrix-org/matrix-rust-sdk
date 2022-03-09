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
//! enabled:
//!
//! 1. `sled_state_store` provides a `StateStore`, while
//! `sled_cryptostore` provides also a `CryptoStore` for encryption data. This
//! is the default persistent store implementation for non-WebAssembly.
//! 2. `indexeddb_store` provides both a `StateStore` and a `CryptoStore` if
//! `encryption` is also enabled. This is the default persistent store
//! implementation for WebAssembly.
//!
//! Both options provide a `make_config` convenience method to create a
//! [`StoreConfig`] for [`ClientConfig::with_store_config()`].
//!
//! [`StoreConfig`]: crate::config::StoreConfig
//! [`ClientConfig::with_store_config()`]: crate::config::ClientConfig::with_store_config()

#[cfg(feature = "indexeddb_stores")]
pub use matrix_sdk_indexeddb::*;
#[cfg(any(feature = "sled_state_store", feature = "sled_cryptostore"))]
pub use matrix_sdk_sled::*;
