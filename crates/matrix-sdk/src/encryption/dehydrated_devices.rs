// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! High-level wrapper for Device Dehydration (MSC3814).

use matrix_sdk_base::crypto::{
    DecryptionSettings, TrustRequirement, store::types::DehydratedDeviceKey,
};
use ruma::api::client::dehydrated_device::{
    delete_dehydrated_device, get_dehydrated_device, get_events,
};
use tracing::{debug, info};

use crate::{Client, Result};

/// A high-level interface to manage dehydrated devices.
#[derive(Debug, Clone)]
pub struct DehydratedDevices {
    pub(crate) client: Client,
}

impl DehydratedDevices {
    /// Creates a new dehydrated device and uploads it to the server.
    ///
    /// This allows the server to store a virtual device that can receive E2EE
    /// keys while the user is offline.
    ///
    /// # Arguments
    ///
    /// * `display_name` - An optional display name for the dehydrated device.
    ///   If `None`, defaults to "Dehydrated device".
    /// * `pickle_key` - The encryption key used to securely encrypt the
    ///   dehydrated device data before uploading it to the server.
    ///
    /// # Errors
    ///
    /// Returns an error if the user is not authenticated, if there is a
    /// cryptographic failure during device creation, or if the server
    /// request fails.
    pub async fn upload(
        &self,
        display_name: Option<&str>,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<()> {
        debug!("Creating a new dehydrated device in the crypto store");
        let machine_guard = self.client.olm_machine().await;
        let machine = machine_guard.as_ref().ok_or(crate::Error::AuthenticationRequired)?;

        let dehydrated_device = machine.dehydrated_devices().create().await?;

        let display_name_str = display_name.unwrap_or("Dehydrated device");

        let upload_req =
            dehydrated_device.keys_for_upload(display_name_str.to_owned(), pickle_key).await?;

        debug!(device_id = ?upload_req.device_id, "Uploading keys to the server");
        self.client.send(upload_req).await?;
        info!("Successfully uploaded dehydrated device");

        Ok(())
    }

    /// Rehydrates a device from the server and absorbs its room keys.
    ///
    /// This fetches the dehydrated device, decrypts its data, fetches the
    /// stored events, and processes them to import the accumulated room
    /// keys into the current session. Finally, the device is deleted from
    /// the server.
    ///
    /// # Arguments
    ///
    /// * `pickle_key` - The decryption key previously used to encrypt the
    ///   dehydrated device data.
    ///
    /// # Errors
    ///
    /// Returns an error if the user is not authenticated, if decryption fails,
    /// or if a server request fails. Returns `Ok(())` (without doing anything)
    /// if the server returns a `404 Not Found`, indicating no dehydrated device
    /// exists.
    pub async fn rehydrate_and_absorb(&self, pickle_key: &DehydratedDeviceKey) -> Result<()> {
        let machine_guard = self.client.olm_machine().await;
        let machine = machine_guard.as_ref().ok_or(crate::Error::AuthenticationRequired)?;

        let request = get_dehydrated_device::unstable::Request::new();
        let response = match self.client.send(request).await {
            Ok(res) => res,
            Err(e) => {
                if let Some(api_err) = e.as_client_api_error() {
                    if api_err.status_code == http::StatusCode::NOT_FOUND {
                        // 404 implies there is no dehydrated device to rehydrate.
                        return Ok(());
                    }
                }
                return Err(e.into());
            }
        };

        let rehydrated = machine
            .dehydrated_devices()
            .rehydrate(pickle_key, &response.device_id, response.device_data)
            .await?;

        let mut next_batch: Option<String> = None;

        let settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

        loop {
            let mut ev_req = get_events::unstable::Request::new(response.device_id.clone());
            ev_req.next_batch = next_batch.clone();

            let ev_res = self.client.send(ev_req).await?;

            if ev_res.events.is_empty() {
                break;
            }

            rehydrated.receive_events(ev_res.events, &settings).await?;

            let received_token = ev_res.next_batch;

            if received_token.is_none() || received_token == next_batch {
                break;
            }

            next_batch = received_token;
        }

        let del_req = delete_dehydrated_device::unstable::Request::new();
        self.client.send(del_req).await?;

        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl DehydratedDevices {
    /// Generates a mock `device_id` and `device_data` payload for integration
    /// tests.
    pub async fn generate_mock_data(
        &self,
        display_name: &str,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<(String, serde_json::Value)> {
        let machine_guard = self.client.olm_machine().await;
        let machine = machine_guard.as_ref().ok_or(crate::Error::AuthenticationRequired)?;

        let dehydrated_device = machine.dehydrated_devices().create().await?;
        let req = dehydrated_device.keys_for_upload(display_name.to_owned(), pickle_key).await?;

        let device_data =
            serde_json::to_value(&req.device_data).expect("Failed to serialize mock device data");

        Ok((req.device_id.to_string(), device_data))
    }
}
