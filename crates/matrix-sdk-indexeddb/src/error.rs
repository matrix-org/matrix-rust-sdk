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
// limitations under the License

use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm};

/// A trait that combines the necessary traits needed for asynchronous runtimes,
/// but excludes them when running in a web environment - i.e., when
/// `#[cfg(target_family = "wasm")]`.
pub trait AsyncErrorDeps: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static {}

impl<T> AsyncErrorDeps for T where T: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static
{}
