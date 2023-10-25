// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use ruma::{
    api::client::account::request_openid_token, authentication::TokenType, OwnedServerName,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct OpenIdState {
    pub(crate) original_request_id: String,
    pub(crate) access_token: String,
    #[serde(with = "ruma::serde::duration::secs")]
    pub(crate) expires_in: Duration,
    pub(crate) matrix_server_name: OwnedServerName,
    pub(crate) token_type: TokenType,
}

impl OpenIdState {
    pub(crate) fn new(id: String, ruma: request_openid_token::v3::Response) -> Self {
        Self {
            original_request_id: id,
            access_token: ruma.access_token,
            expires_in: ruma.expires_in,
            matrix_server_name: ruma.matrix_server_name,
            token_type: ruma.token_type,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "state")]
pub(crate) enum OpenIdResponse {
    Allowed(OpenIdState),
    Blocked {
        original_request_id: String,
    },
    #[serde(rename = "request")]
    Pending,
}
