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
//! 1. The `sled` feature provides a `StateStore` for storing state and
//!    a `CryptoStore` for E2EE data (if `e2e-encryption` is enabled). This is
//!    the default persistent store implementation for non-WebAssembly targets.
//! 2. The `indexeddb` feature also provides a `StateStore` for storing state
//!    and a `CryptoStore` (if `e2e-encryption` is enabled). This is the default
//!    persistent store implementation for WebAssembly targets.
//!
//! Both options provide a `make_store_config` convenience method to create a
//! [`StoreConfig`] for [`ClientBuilder::store_config()`].
//!
//! [`StoreConfig`]: crate::config::StoreConfig
//! [`ClientBuilder::store_config()`]: crate::ClientBuilder::store_config

#[cfg(feature = "indexeddb")]
pub use matrix_sdk_indexeddb::*;
#[cfg(feature = "sled")]
pub use matrix_sdk_sled::*;
