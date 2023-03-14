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

//! Collection of public identities used in Matrix.
//!
//! Matrix supports two main types of identities, a per-device identity and a
//! per-user identity.
//!
//! ## Device
//!
//! Every E2EE capable Matrix client will create a new Olm account and upload
//! the public keys of the Olm account to the server. This is represented as a
//! `ReadOnlyDevice`.
//!
//! Devices can have a local trust state which is needs to be saved in our
//! `CryptoStore`, to avoid reference cycles a wrapper for the `ReadOnlyDevice`
//! exists which adds methods to manipulate the local trust state.
//!
//! ## User
//!
//! Cross-signing capable devices will upload 3 additional (master,
//! self-signing, user-signing) public keys which represent the user identity
//! owning all the devices. This is represented in two ways, as a `UserIdentity`
//! for other users and as `OwnUserIdentity` for our own user.
//!
//! This is done because the server will only give us access to 2 of the 3
//! additional public keys for other users, while it will give us access to all
//! 3 for our own user.
//!
//! Both identity sets need to regularly fetched from the server using the
//! `/keys/query` API call.
pub(crate) mod device;
pub(crate) mod manager;
pub(crate) mod user;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub use device::{Device, LocalTrust, ReadOnlyDevice, UserDevices};
pub(crate) use manager::IdentityManager;
use serde::{Deserialize, Deserializer, Serializer};
pub use user::{
    OwnUserIdentity, ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities, ReadOnlyUserIdentity,
    UserIdentities, UserIdentity,
};

// These methods are only here because Serialize and Deserialize don't seem to
// be implemented for WASM.
fn atomic_bool_serializer<S>(x: &AtomicBool, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let value = x.load(Ordering::SeqCst);
    s.serialize_some(&value)
}

fn atomic_bool_deserializer<'de, D>(deserializer: D) -> Result<Arc<AtomicBool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    Ok(Arc::new(AtomicBool::new(value)))
}
