// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Runtime abstractions for cross-platform async execution. This provides
//! a stand-in for tokio's `get_runtime_handle` method that will work on
//! both Wasm and non-Wasm platforms. It also provides corresponding types
//! that can be used in place of tokio's `Handle` and `Runtime` types.

#[cfg(not(target_family = "wasm"))]
mod sys {
    pub use tokio::runtime::Handle;

    /// Get a runtime handle appropriate for the current target platform.
    ///
    /// This function returns a unified `Handle` type that works across both
    /// Wasm and non-Wasm platforms, allowing code to be written that is
    /// agnostic to the platform-specific runtime implementation.
    ///
    /// Returns:
    /// - A `tokio::runtime::Handle` on non-Wasm platforms
    /// - A `WasmRuntimeHandle` on Wasm platforms
    pub fn get_runtime_handle() -> Handle {
        async_compat::get_runtime_handle()
    }
}

#[cfg(target_family = "wasm")]
mod sys {
    use std::future::Future;

    use matrix_sdk_common::executor::{spawn, JoinHandle};

    /// A dummy guard that does nothing when dropped.
    /// This is used for the Wasm implementation to match
    /// tokio::runtime::EnterGuard.
    #[derive(Debug)]
    pub struct RuntimeGuard;

    /// A runtime handle implementation for WebAssembly targets.
    ///
    /// This implements a minimal subset of the tokio::runtime::Handle API
    /// that is needed for the matrix-rust-sdk to function on Wasm.
    #[derive(Default, Debug)]
    pub struct Handle;
    pub type Runtime = Handle;

    impl Handle {
        /// Spawns a future in the wasm32 bindgen runtime.
        #[track_caller]
        pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
        where
            F: Future + 'static,
            F::Output: 'static,
        {
            spawn(future)
        }

        /// Runs the provided function on an executor dedicated to blocking
        /// operations.
        #[track_caller]
        pub fn spawn_blocking<F, R>(&self, func: F) -> JoinHandle<R>
        where
            F: FnOnce() -> R + 'static,
            R: 'static,
        {
            spawn(async move { func() })
        }

        /// Runs a future to completion on the current thread.
        pub fn block_on<F, T>(&self, future: F) -> T
        where
            F: Future<Output = T>,
        {
            futures_executor::block_on(future)
        }

        /// Enters the runtime context.
        ///
        /// For WebAssembly, this is a no-op that returns a dummy guard.
        pub fn enter(&self) -> RuntimeGuard {
            RuntimeGuard
        }
    }

    /// Get a runtime handle appropriate for the current target platform.
    ///
    /// This function returns a unified `Handle` type that works across both
    /// Wasm and non-Wasm platforms, allowing code to be written that is
    /// agnostic to the platform-specific runtime implementation.
    ///
    /// Returns:
    /// - A `tokio::runtime::Handle` on non-Wasm platforms
    /// - A `WasmRuntimeHandle` on Wasm platforms
    pub fn get_runtime_handle() -> Handle {
        Handle
    }
}

pub use sys::*;
