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

#[cfg(feature = "experimental-oidc")]
use matrix_sdk_base::ttl_cache::TtlCache;
#[cfg(feature = "experimental-oidc")]
use ruma::api::client::discovery::get_authorization_server_metadata::msc2965::AuthorizationServerMetadata;
use tokio::sync::RwLock;

use super::ClientServerCapabilities;

/// A collection of in-memory data that the `Client` might want to cache to
/// avoid hitting the homeserver every time users request the data.
pub(crate) struct ClientCaches {
    /// Server capabilities, either prefilled during building or fetched from
    /// the server.
    pub(super) server_capabilities: RwLock<ClientServerCapabilities>,
    #[cfg(feature = "experimental-oidc")]
    pub(crate) provider_metadata: tokio::sync::Mutex<TtlCache<String, AuthorizationServerMetadata>>,
}
