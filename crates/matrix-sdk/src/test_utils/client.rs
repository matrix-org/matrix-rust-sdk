// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Augmented [`ClientBuilder`] that can set up an already logged-in user.

use matrix_sdk_base::{store::StoreConfig, SessionMeta};
use ruma::{api::MatrixVersion, device_id, user_id};

use crate::{
    authentication::matrix::{MatrixSession, MatrixSessionTokens},
    config::RequestConfig,
    Client, ClientBuilder,
};

/// An augmented [`ClientBuilder`] that also allows for handling session login.
#[allow(missing_debug_implementations)]
pub struct MockClientBuilder {
    builder: ClientBuilder,
    logged_in: bool,
}

impl MockClientBuilder {
    /// Create a new [`MockClientBuilder`] connected to the given homeserver,
    /// using Matrix V1.12, and which will not attempt any network retry (by
    /// default).
    pub(crate) fn new(homeserver: String) -> Self {
        let default_builder = Client::builder()
            .homeserver_url(homeserver)
            .server_versions([MatrixVersion::V1_12])
            .request_config(RequestConfig::new().disable_retry());

        Self { builder: default_builder, logged_in: true }
    }

    /// Doesn't log-in a user.
    ///
    /// Authenticated requests will fail if this is called.
    pub fn unlogged(mut self) -> Self {
        self.logged_in = false;
        self
    }

    /// Provides another [`StoreConfig`] for the underlying [`ClientBuilder`].
    pub fn store_config(mut self, store_config: StoreConfig) -> Self {
        self.builder = self.builder.store_config(store_config);
        self
    }

    /// Finish building the client into the final [`Client`] instance.
    pub async fn build(self) -> Client {
        let client = self.builder.build().await.expect("building client failed");

        if self.logged_in {
            client
                .matrix_auth()
                .restore_session(MatrixSession {
                    meta: SessionMeta {
                        user_id: user_id!("@example:localhost").to_owned(),
                        device_id: device_id!("DEVICEID").to_owned(),
                    },
                    tokens: MatrixSessionTokens {
                        access_token: "1234".to_owned(),
                        refresh_token: None,
                    },
                })
                .await
                .unwrap();
        }

        client
    }
}
