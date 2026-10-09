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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{future::pending, sync::Arc, time::Duration};

use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{UserId, api::client::presence::get_presence, events::presence::PresenceEvent};

use crate::{
    client::Client, error::ClientError, ruma::PresenceState, runtime::get_runtime_handle,
    task_handle::TaskHandle,
};

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct UserPresence {
    pub user_id: String,
    pub presence: PresenceState,
    pub status_msg: Option<String>,
    pub last_active_ago: Option<Duration>,
    pub currently_active: Option<bool>,
}

impl From<PresenceEvent> for UserPresence {
    fn from(event: PresenceEvent) -> Self {
        Self {
            user_id: event.sender.to_string(),
            presence: event.content.presence.into(),
            status_msg: event.content.status_msg,
            last_active_ago: event
                .content
                .last_active_ago
                .map(|millis| Duration::from_millis(millis.into())),
            currently_active: event.content.currently_active,
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait PresenceListener: SendOutsideWasm + SyncOutsideWasm {
    fn on_update(&self, presence: UserPresence);
}

#[matrix_sdk_ffi_macros::export]
impl Client {
    pub async fn set_presence_with_status(
        &self,
        presence: PresenceState,
        status_msg: Option<String>,
        immediate: bool,
    ) -> Result<(), ClientError> {
        Ok(self.inner.set_presence(presence.into(), status_msg, immediate).await?)
    }

    pub async fn get_user_presence(&self, user_id: String) -> Result<UserPresence, ClientError> {
        let user_id = UserId::parse(user_id)?;
        let response = self.inner.send(get_presence::v3::Request::new(user_id.clone())).await?;
        Ok(UserPresence {
            user_id: user_id.to_string(),
            presence: response.presence.into(),
            status_msg: response.status_msg,
            last_active_ago: response.last_active_ago,
            currently_active: response.currently_active,
        })
    }

    pub fn subscribe_to_presence_updates(
        &self,
        listener: Box<dyn PresenceListener>,
    ) -> Arc<TaskHandle> {
        let listener: Arc<dyn PresenceListener> = listener.into();
        let handle = self.inner.add_event_handler(move |event: PresenceEvent| {
            let listener = listener.clone();
            async move { listener.on_update(event.into()) }
        });
        let guard = self.inner.event_handler_drop_guard(handle);
        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            let _guard = guard;
            pending::<()>().await;
        })))
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_common::cross_process_lock::CrossProcessLockConfig;
    use ruma::serde::Raw;
    use serde_json::json;
    use tokio::{sync::mpsc, time::timeout};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{body_json, header, method, path_regex},
    };

    use super::*;

    #[test]
    fn maps_presence_without_inventing_optional_fields() {
        for (value, expected) in [
            ("online", PresenceState::Online),
            ("offline", PresenceState::Offline),
            ("unavailable", PresenceState::Unavailable),
        ] {
            let event: PresenceEvent = serde_json::from_value(json!({
                "type": "m.presence", "sender": "@alice:example.org",
                "content": {"presence": value}
            }))
            .unwrap();
            let presence = UserPresence::from(event);
            assert_eq!(presence.user_id, "@alice:example.org");
            assert_eq!(presence.presence, expected);
            assert_eq!(presence.status_msg, None);
            assert_eq!(presence.last_active_ago, None);
            assert_eq!(presence.currently_active, None);
        }
    }

    #[tokio::test]
    async fn fetches_presence_with_activity_and_status() {
        let server = MatrixMockServer::new().await;
        let sdk_client = server
            .client_builder()
            .on_builder(|builder| {
                builder.cross_process_store_config(CrossProcessLockConfig::SingleProcess)
            })
            .build()
            .await;
        let client = Client::new(sdk_client, None, None).await.unwrap();
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/(r0|v3)/presence/.*/status$"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "presence": "unavailable", "status_msg": "At lunch",
                "last_active_ago": 12345, "currently_active": false
            })))
            .expect(1)
            .mount(server.server())
            .await;
        let presence = client.get_user_presence("@alice:example.org".to_owned()).await.unwrap();
        assert_eq!(presence.user_id, "@alice:example.org");
        assert_eq!(presence.presence, PresenceState::Unavailable);
        assert_eq!(presence.status_msg.as_deref(), Some("At lunch"));
        assert_eq!(presence.last_active_ago, Some(Duration::from_millis(12345)));
        assert_eq!(presence.currently_active, Some(false));
    }

    #[tokio::test]
    async fn publishes_presence_with_an_existing_status_message() {
        let server = MatrixMockServer::new().await;
        let sdk_client = server
            .client_builder()
            .on_builder(|builder| {
                builder.cross_process_store_config(CrossProcessLockConfig::SingleProcess)
            })
            .build()
            .await;
        let client = Client::new(sdk_client, None, None).await.unwrap();
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/(r0|v3)/presence/.*/status$"))
            .and(header("authorization", "Bearer 1234"))
            .and(body_json(json!({"presence": "unavailable", "status_msg": "At lunch"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(server.server())
            .await;
        client
            .set_presence_with_status(PresenceState::Unavailable, Some("At lunch".into()), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reports_invalid_ids_and_server_errors() {
        let server = MatrixMockServer::new().await;
        let sdk_client = server
            .client_builder()
            .on_builder(|builder| {
                builder.cross_process_store_config(CrossProcessLockConfig::SingleProcess)
            })
            .build()
            .await;
        let client = Client::new(sdk_client, None, None).await.unwrap();
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/(r0|v3)/presence/.*/status$"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "errcode": "M_FORBIDDEN", "error": "Presence is not shared"
            })))
            .expect(1)
            .mount(server.server())
            .await;
        assert!(client.get_user_presence("not-a-user-id".to_owned()).await.is_err());
        assert!(client.get_user_presence("@alice:example.org".to_owned()).await.is_err());
    }

    struct Listener(mpsc::UnboundedSender<UserPresence>);

    impl PresenceListener for Listener {
        fn on_update(&self, presence: UserPresence) {
            self.0.send(presence).unwrap();
        }
    }

    #[tokio::test]
    async fn subscription_delivers_each_user_and_releases_listener_on_cancel_or_drop() {
        for cancel in [false, true] {
            let server = MatrixMockServer::new().await;
            let sdk_client = server
                .client_builder()
                .on_builder(|builder| {
                    builder.cross_process_store_config(CrossProcessLockConfig::SingleProcess)
                })
                .build()
                .await;
            let client = Client::new(sdk_client.clone(), None, None).await.unwrap();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let task = client.subscribe_to_presence_updates(Box::new(Listener(sender)));
            server.mock_sync().ok_and_run(&sdk_client, |builder| {
                builder.add_presence_bulk(["@alice:example.org", "@bob:example.org"].map(|sender| {
                    Raw::new(&json!({
                        "type": "m.presence", "sender": sender,
                        "content": {"presence": "online", "currently_active": true, "last_active_ago": 0}
                    })).unwrap().cast_unchecked()
                }));
            }).await;
            let mut received = Vec::new();
            for _ in 0..2 {
                let presence =
                    timeout(Duration::from_secs(5), receiver.recv()).await.unwrap().unwrap();
                assert_eq!(presence.presence, PresenceState::Online);
                assert_eq!(presence.currently_active, Some(true));
                assert_eq!(presence.last_active_ago, Some(Duration::ZERO));
                received.push(presence.user_id);
            }
            received.sort();
            assert_eq!(received, ["@alice:example.org", "@bob:example.org"]);
            if cancel {
                task.cancel();
                assert_eq!(timeout(Duration::from_secs(5), receiver.recv()).await.unwrap(), None);
            } else {
                drop(task);
                assert_eq!(timeout(Duration::from_secs(5), receiver.recv()).await.unwrap(), None);
            }
        }
    }
}
