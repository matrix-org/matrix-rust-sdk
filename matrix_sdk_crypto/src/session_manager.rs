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

use std::{collections::BTreeMap, time::Duration};

use matrix_sdk_common::{
    api::r0::keys::claim_keys::{Request as KeysClaimRequest, Response as KeysClaimResponse},
    assign,
    identifiers::{DeviceKeyAlgorithm, UserId},
    uuid::Uuid,
};
use tracing::{error, info, warn};

use crate::{error::OlmResult, key_request::KeyRequestMachine, olm::Account, store::Store};

#[derive(Debug, Clone)]
pub(crate) struct SessionManager {
    account: Account,
    store: Store,
    key_request_machine: KeyRequestMachine,
}

impl SessionManager {
    const KEY_CLAIM_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(account: Account, key_request_machine: KeyRequestMachine, store: Store) -> Self {
        Self {
            account,
            store,
            key_request_machine,
        }
    }

    pub async fn get_missing_sessions(
        &self,
        users: &mut impl Iterator<Item = &UserId>,
    ) -> OlmResult<Option<(Uuid, KeysClaimRequest)>> {
        let mut missing = BTreeMap::new();

        // Add the list of devices that the user wishes to establish sessions
        // right now.
        for user_id in users {
            let user_devices = self.store.get_user_devices(user_id).await?;

            for device in user_devices.devices() {
                let sender_key = if let Some(k) = device.get_key(DeviceKeyAlgorithm::Curve25519) {
                    k
                } else {
                    continue;
                };

                let sessions = self.store.get_sessions(sender_key).await?;

                let is_missing = if let Some(sessions) = sessions {
                    sessions.lock().await.is_empty()
                } else {
                    true
                };

                if is_missing {
                    missing
                        .entry(user_id.to_owned())
                        .or_insert_with(BTreeMap::new)
                        .insert(
                            device.device_id().into(),
                            DeviceKeyAlgorithm::SignedCurve25519,
                        );
                }
            }
        }

        // Add the list of sessions that for some reason automatically need to
        // create an Olm session.
        for item in self.key_request_machine.users_for_key_claim().iter() {
            let user = item.key();

            for device_id in item.value().iter() {
                missing
                    .entry(user.to_owned())
                    .or_insert_with(BTreeMap::new)
                    .insert(device_id.to_owned(), DeviceKeyAlgorithm::SignedCurve25519);
            }
        }

        if missing.is_empty() {
            Ok(None)
        } else {
            Ok(Some((
                Uuid::new_v4(),
                assign!(KeysClaimRequest::new(missing), {
                    timeout: Some(Self::KEY_CLAIM_TIMEOUT),
                }),
            )))
        }
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `response` - The response containing the claimed one-time keys.
    pub async fn receive_keys_claim_response(&self, response: &KeysClaimResponse) -> OlmResult<()> {
        // TODO log the failures here

        for (user_id, user_devices) in &response.one_time_keys {
            for (device_id, key_map) in user_devices {
                let device = match self.store.get_readonly_device(&user_id, device_id).await {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but the device is unknown",
                            user_id, device_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but \
                            can't fetch the device from the store {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                };

                info!("Creating outbound Session for {} {}", user_id, device_id);

                let session = match self.account.create_outbound_session(device, &key_map).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("{:?}", e);
                        continue;
                    }
                };

                if let Err(e) = self.store.save_sessions(&[session]).await {
                    error!("Failed to store newly created Olm session {}", e);
                    continue;
                }

                // TODO if this session was created because a previous one was
                // wedged queue up a dummy event to be sent out.
                self.key_request_machine.retry_keyshare(&user_id, device_id);
            }
        }
        Ok(())
    }
}
