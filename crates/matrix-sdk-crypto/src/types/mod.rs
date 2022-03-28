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

//! Module containing customized types modeling Matrix keys.
//!
//! These types were mostly taken from the Ruma project. The types differ in two
//! important ways to the Ruma types of the same name:
//!
//! 1. They are using vodozemac types so we directly deserialize into a
//!    vodozemac curve25519 or ed25519 key.
//! 2. They support lossless serialization cycles in a canonical JSON supported
//!    way, meaning the white-space and field order won't be preserved but the
//!    data will.

mod cross_signing_key;
mod device_keys;
mod one_time_keys;

pub use cross_signing_key::*;
pub use device_keys::*;
pub use one_time_keys::*;
