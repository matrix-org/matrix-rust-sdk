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

use std::{ops::Deref, result::Result as StdResult};

use matrix_sdk_base::crypto::{
    store::CryptoStoreError, Device as BaseDevice, LocalTrust, ReadOnlyDevice,
    UserDevices as BaseUserDevices,
};
use matrix_sdk_common::identifiers::{DeviceId, DeviceIdBox};

use crate::{error::Result, Client, Sas};

#[derive(Clone, Debug)]
/// A device represents a E2EE capable client of an user.
pub struct Device {
    pub(crate) inner: BaseDevice,
    pub(crate) client: Client,
}

impl Deref for Device {
    type Target = ReadOnlyDevice;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Device {
    /// Start a interactive verification with this `Device`
    ///
    /// Returns a `Sas` object that represents the interactive verification flow.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, identifiers::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let device = client.get_device(&alice, "DEVICEID".into())
    ///     .await
    ///     .unwrap()
    ///     .unwrap();
    ///
    /// let verification = device.start_verification().await.unwrap();
    /// # });
    /// ```
    pub async fn start_verification(&self) -> Result<Sas> {
        let (sas, request) = self.inner.start_verification().await?;
        self.client.send_to_device(&request).await?;

        Ok(Sas { inner: sas, client: self.client.clone() })
    }

    /// Is the device trusted.
    pub fn is_trusted(&self) -> bool {
        self.inner.trust_state()
    }

    /// Set the local trust state of the device to the given state.
    ///
    /// This won't affect any cross signing trust state, this only sets a flag
    /// marking to have the given trust state.
    ///
    /// # Arguments
    ///
    /// * `trust_state` - The new trust state that should be set for the device.
    pub async fn set_local_trust(
        &self,
        trust_state: LocalTrust,
    ) -> StdResult<(), CryptoStoreError> {
        self.inner.set_local_trust(trust_state).await
    }
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: BaseUserDevices,
    pub(crate) client: Client,
}

impl UserDevices {
    /// Get the specific device with the given device id.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.inner.get(device_id).map(|d| Device { inner: d, client: self.client.clone() })
    }

    /// Iterator over all the device ids of the user devices.
    pub fn keys(&self) -> impl Iterator<Item = &DeviceIdBox> {
        self.inner.keys()
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> impl Iterator<Item = Device> + '_ {
        let client = self.client.clone();

        self.inner.devices().map(move |d| Device { inner: d, client: client.clone() })
    }
}
