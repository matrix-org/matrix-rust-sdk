// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! This is the encryption part of the matrix-sdk. It contains a state machine
//! that will aid in adding encryption support to a client library.

#![deny(
    missing_debug_implementations,
    dead_code,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]

mod device;
mod error;
mod machine;
mod memory_stores;
mod olm;
mod store;

pub use device::{Device, TrustState};
pub use error::{MegolmError, OlmError};
pub use machine::{OlmMachine, OneTimeKeys};
pub use memory_stores::{DeviceStore, GroupSessionStore, SessionStore, UserDevices};
pub use olm::{Account, InboundGroupSession, OutboundGroupSession, Session};
#[cfg(feature = "sqlite-cryptostore")]
pub use store::sqlite::SqliteStore;
pub use store::{CryptoStore, CryptoStoreError};
