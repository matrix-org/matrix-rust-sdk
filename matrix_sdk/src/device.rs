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
use ruma::{events::key::verification::VerificationMethod, DeviceId, DeviceIdBox};

use crate::{
    error::Result,
    verification::{SasVerification, VerificationRequest},
    Client,
};

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
    /// Returns a `Sas` object that represents the interactive verification
    /// flow.
    ///
    /// This method has been deprecated in the spec and the
    /// [`request_verification()`] method should be used instead.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
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
    ///
    /// [`request_verification()`]: #method.request_verification
    pub async fn start_verification(&self) -> Result<SasVerification> {
        let (sas, request) = self.inner.start_verification().await?;
        self.client.send_to_device(&request).await?;

        Ok(SasVerification { inner: sas, client: self.client.clone() })
    }

    /// Request an interacitve verification with this `Device`
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    ///
    /// The default methods that are supported are `m.sas.v1` and
    /// `m.qr_code.show.v1`, if this isn't desireable the
    /// [`request_verification_with_methods()`] method can be used to override
    /// this.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
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
    /// let verification = device.request_verification().await.unwrap();
    /// # });
    /// ```
    ///
    /// [`request_verification_with_methods()`]:
    /// #method.request_verification_with_methods
    pub async fn request_verification(&self) -> Result<VerificationRequest> {
        let (verification, request) = self.inner.request_verification().await;
        self.client.send_verification_request(request).await?;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    /// Request an interacitve verification with this `Device`
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    ///
    /// # Arguments
    ///
    /// * `methods` - The verification methods that we want to support.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{
    /// #    Client,
    /// #    ruma::{
    /// #        UserId,
    /// #        events::key::verification::VerificationMethod,
    /// #    }
    /// # };
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
    /// // We don't want to support showing a QR code, we only support SAS
    /// // verification
    /// let methods = vec![VerificationMethod::SasV1];
    ///
    /// let verification = device.request_verification_with_methods(methods).await.unwrap();
    /// # });
    /// ```
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Result<VerificationRequest> {
        let (verification, request) = self.inner.request_verification_with_methods(methods).await;
        self.client.send_verification_request(request).await?;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    /// Is the device considered to be verified, either by locally trusting it
    /// or using cross signing.
    pub fn verified(&self) -> bool {
        self.inner.verified()
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
