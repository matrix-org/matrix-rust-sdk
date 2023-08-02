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

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json,
};
use axum_extra::response::ErasedJson;
use matrix_sdk::{
    config::RequestConfig,
    matrix_auth::{Session, SessionTokens},
    Client, ClientBuilder,
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{test_json, SyncResponseBuilder};
use ruma::{api::MatrixVersion, device_id, user_id};
use tokio::spawn;

pub fn test_client_builder(routes: axum::Router) -> ClientBuilder {
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
    let server = axum::Server::bind(&addr).serve(routes.into_make_service());

    let client_builder = Client::builder()
        .homeserver_url(format!("http://{}", server.local_addr()))
        .server_versions([MatrixVersion::V1_7]);

    spawn(async move {
        server.await.unwrap();
    });

    client_builder
}

pub async fn no_retry_test_client(routes: axum::Router) -> Client {
    test_client_builder(routes)
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap()
}

pub async fn logged_in_client(routes: axum::Router) -> Client {
    let session = Session {
        meta: SessionMeta {
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: SessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };

    let client = no_retry_test_client(routes).await;
    client.restore_session(session).await.unwrap();
    client
}

pub trait RouterExt: Sized {
    fn mock_encryption_state(self, is_encrypted: bool) -> Self;
    fn mock_sync_responses(self, sync_builder: SyncResponseBuilder) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RouterExt for axum::Router<S> {
    fn mock_encryption_state(self, is_encrypted: bool) -> Self {
        self.route(
            "/_matrix/client/v3/rooms/:room_id/state/m.room.encryption/",
            if is_encrypted {
                get(ErasedJson::new(&*test_json::sync_events::ENCRYPTION_CONTENT))
            } else {
                get((StatusCode::NOT_FOUND, ErasedJson::new(&*test_json::NOT_FOUND)))
            },
        )
    }

    fn mock_sync_responses(self, sync_builder: SyncResponseBuilder) -> Self {
        self.route(
            "/_matrix/client/v3/sync",
            get(move || async move { Json(sync_builder.build_json_sync_response()) }),
        )
    }
}

#[derive(Clone, Default)]
pub struct ResponseVar {
    inner: Arc<Mutex<Option<Response>>>,
}

impl ResponseVar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, r: impl IntoResponse) {
        *self.inner.lock().unwrap() = Some(r.into_response());
    }
}

impl IntoResponse for ResponseVar {
    fn into_response(self) -> Response {
        self.inner.lock().unwrap().take().unwrap_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, "ResponseVar not set").into_response()
        })
    }
}
