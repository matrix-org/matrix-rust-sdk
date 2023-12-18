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

#[doc(no_inline)]
pub use instant;
#[doc(no_inline)]
pub use ruma;

pub mod debug;
pub mod deserialized_responses;
pub mod executor;
pub mod failures_cache;
pub mod ring_buffer;
pub mod store_locks;
pub mod timeout;
pub mod tracing_timer;

// We cannot currently measure test coverage in the WASM environment, so
// js_tracing is incorrectly flagged as untested. Disable coverage checking for
// it.
#[cfg(all(target_arch = "wasm32", not(tarpaulin_include)))]
pub mod js_tracing;

pub use store_locks::LEASE_DURATION_MS;

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_arch = "wasm32"))]
pub trait SendOutsideWasm: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> SendOutsideWasm for T {}

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_arch = "wasm32")]
pub trait SendOutsideWasm {}
#[cfg(target_arch = "wasm32")]
impl<T> SendOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_arch = "wasm32"))]
pub trait SyncOutsideWasm: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync> SyncOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_arch = "wasm32")]
pub trait SyncOutsideWasm {}
#[cfg(target_arch = "wasm32")]
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
        #[cfg(target_arch = "wasm32")]
        type IntoFuture = ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = Self::Output> + $($extra_bounds)*
        >>;
        #[cfg(not(target_arch = "wasm32"))]
        type IntoFuture = ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = Self::Output> + Send + $($extra_bounds)*
        >>;
    };
}
