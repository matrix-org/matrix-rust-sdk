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

#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations)]

use std::pin::Pin;

#[cfg(test)]
matrix_sdk_test_utils::init_tracing_for_tests!();

use futures_core::Future;
#[doc(no_inline)]
pub use ruma;

pub mod cross_process_lock;
pub mod debug;
pub mod deserialized_responses;
pub mod executor;
pub mod failures_cache;
pub mod linked_chunk;
pub mod locks;
pub mod ring_buffer;
pub mod serde_helpers;
pub mod sleep;
pub mod stream;
pub mod timeout;
pub mod tracing_timer;
pub mod ttl_cache;

// We cannot currently measure test coverage in the WASM environment, so
// js_tracing is incorrectly flagged as untested. Disable coverage checking for
// it.
#[cfg(all(target_family = "wasm", not(tarpaulin_include)))]
pub mod js_tracing;

pub use cross_process_lock::LEASE_DURATION_MS;
use ruma::{RoomVersionId, room_version_rules::RoomVersionRules};

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_family = "wasm"))]
pub trait SendOutsideWasm: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> SendOutsideWasm for T {}

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_family = "wasm")]
pub trait SendOutsideWasm {}
#[cfg(target_family = "wasm")]
impl<T> SendOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_family = "wasm"))]
pub trait SyncOutsideWasm: Sync {}
#[cfg(not(target_family = "wasm"))]
impl<T: Sync> SyncOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_family = "wasm")]
pub trait SyncOutsideWasm {}
#[cfg(target_family = "wasm")]
impl<T> SyncOutsideWasm for T {}

/// Super trait that is used for our store traits, this trait will differ if
/// it's used on WASM. WASM targets will not require `Send` and `Sync` to have
/// implemented, while other targets will.
pub trait AsyncTraitDeps: std::fmt::Debug + SendOutsideWasm + SyncOutsideWasm {}
impl<T: std::fmt::Debug + SendOutsideWasm + SyncOutsideWasm> AsyncTraitDeps for T {}

// TODO: Remove in favor of impl Trait once allowed in associated types
#[macro_export]
macro_rules! boxed_into_future {
    () => {
        $crate::boxed_into_future!(extra_bounds: );
    };
    (extra_bounds: $($extra_bounds:tt)*) => {
        #[cfg(target_family = "wasm")]
        type IntoFuture = ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = Self::Output> + $($extra_bounds)*
        >>;
        #[cfg(not(target_family = "wasm"))]
        type IntoFuture = ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = Self::Output> + Send + $($extra_bounds)*
        >>;
    };
}

/// A `Box::pin` future that is `Send` on non-wasm, and without `Send` on wasm.
#[cfg(target_family = "wasm")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
#[cfg(not(target_family = "wasm"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

/// The room version to use as a fallback when the version of a room is unknown.
pub const ROOM_VERSION_FALLBACK: RoomVersionId = RoomVersionId::V11;

/// The room version rules to use as a fallback when the version of a room is
/// unknown or unsupported.
///
/// These are the rules of the [`ROOM_VERSION_FALLBACK`].
pub const ROOM_VERSION_RULES_FALLBACK: RoomVersionRules = RoomVersionRules::V11;
