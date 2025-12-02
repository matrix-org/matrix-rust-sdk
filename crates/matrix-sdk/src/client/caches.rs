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

use matrix_sdk_base::{store::WellKnownResponse, ttl_cache::TtlCache};
use ruma::api::{
    SupportedVersions,
    client::discovery::get_authorization_server_metadata::v1::AuthorizationServerMetadata,
};
use tokio::sync::RwLock;

/// A collection of in-memory data that the `Client` might want to cache to
/// avoid hitting the homeserver every time users request the data.
pub(crate) struct ClientCaches {
    /// The supported versions of the homeserver.
    ///
    /// We only want to cache:
    ///
    /// - The versions prefilled with `ClientBuilder::server_versions()`
    /// - The versions fetched from an *authenticated* request to the server.
    pub(crate) supported_versions: RwLock<CachedValue<SupportedVersions>>,
    /// Well-known information.
    pub(super) well_known: RwLock<CachedValue<Option<WellKnownResponse>>>,
    pub(crate) server_metadata: tokio::sync::Mutex<TtlCache<String, AuthorizationServerMetadata>>,
}

/// A cached value that can either be set or not set, used to avoid confusion
/// between a value that is set to `None` (because it doesn't exist) and a value
/// that has not been cached yet.
#[derive(Clone)]
pub(crate) enum CachedValue<Value> {
    /// A value has been cached.
    Cached(Value),
    /// Nothing has been cached yet.
    NotSet,
}

impl<Value> CachedValue<Value> {
    /// Takes the value out of the `CachedValue`, leaving a `NotSet` in its
    /// place.
    pub(super) fn take(&mut self) -> Option<Value> {
        let prev = std::mem::replace(self, Self::NotSet);

        match prev {
            Self::Cached(value) => Some(value),
            Self::NotSet => None,
        }
    }
}
