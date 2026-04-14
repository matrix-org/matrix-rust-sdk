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

use std::convert::Infallible;

use deadpool::managed;
pub use deadpool::managed::reexports::*;

#[cfg(not(target_family = "wasm"))]
mod default;
#[cfg(target_family = "wasm")]
mod wasm;

#[cfg(not(target_family = "wasm"))]
pub use default::*;
#[cfg(target_family = "wasm")]
pub use wasm::*;

/// The default runtime used by `matrix-sdk-sqlite` for `deadpool`.
pub const RUNTIME: Runtime = Runtime::Tokio1;

deadpool::managed_reexports!(
    "matrix-sdk-sqlite",
    Manager,
    managed::Object<Manager>,
    rusqlite::Error,
    Infallible
);

/// Type representing a connection to SQLite from the [`Pool`].
pub type Connection = Object;
