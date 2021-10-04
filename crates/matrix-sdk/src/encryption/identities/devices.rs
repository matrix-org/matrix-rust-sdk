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

use super::ManualVerifyError;
use crate::{
    encryption::verification::{SasVerification, VerificationRequest},
    error::Result,
    Client,
};

/// A device represents a E2EE capable client or device of an user.
///
/// A device is backed by [device keys] that are uploaded to the server.
///
/// [device keys]: https://spec.matrix.org/unstable/client-server-api/#device-keys
#[derive(Clone, Debug)]
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
    /// Request an interactive verification with this `Device`.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// The default methods that are supported are `m.sas.v1` and
    /// `m.qr_code.show.v1`, if this isn't desirable the
    /// [`request_verification_with_methods()`] method can be used to override
    /// this. `m.qr_code.show.v1` is only avaliable if the `qrcode` feature is
    /// enabled, which it is by default.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = UserId::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let device = client.get_device(&alice, "DEVICEID".into()).await?;
    ///
    /// if let Some(device) = device {
    ///     let verification = device.request_verification().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    ///
    /// [`request_verification_with_methods()`]:
    /// #method.request_verification_with_methods
    pub async fn request_verification(&self) -> Result<VerificationRequest> {
        let (verification, request) = self.inner.request_verification().await;
        self.client.send_verification_request(request).await?;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    /// Request an interactive verification with this `Device`.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// # Arguments
    ///
    /// * `methods` - The verification methods that we want to support. Must be
    /// non-empty.
    ///
    /// # Panics
    ///
    /// This method will panic if `methods` is empty.
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
    /// # block_on(async {
    /// # let alice = UserId::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let device = client.get_device(&alice, "DEVICEID".into()).await?;
    ///
    /// // We don't want to support showing a QR code, we only support SAS
    /// // verification
    /// let methods = vec![VerificationMethod::SasV1];
    ///
    /// if let Some(device) = device {
    ///     let verification = device.request_verification_with_methods(methods).await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Result<VerificationRequest> {
        assert!(!methods.is_empty(), "The list of verification methods can't be non-empty");

        let (verification, request) = self.inner.request_verification_with_methods(methods).await;
        self.client.send_verification_request(request).await?;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    /// Start an interactive verification with this [`Device`]
    ///
    /// Returns a [`SasVerification`] object that represents the interactive
    /// verification flow.
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
    /// # block_on(async {
    /// # let alice = UserId::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let device = client.get_device(&alice, "DEVICEID".into()).await?;
    ///
    /// if let Some(device) = device {
    ///     let verification = device.start_verification().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    ///
    /// [`request_verification()`]: #method.request_verification
    #[deprecated(
        since = "0.4.0",
        note = "directly starting a verification is deprecated in the spec. \
                Users should instead use request_verification()"
    )]
    pub async fn start_verification(&self) -> Result<SasVerification> {
        let (sas, request) = self.inner.start_verification().await?;
        self.client.send_to_device(&request).await?;

        Ok(SasVerification { inner: sas, client: self.client.clone() })
    }

    /// Manually verify this device.
    ///
    /// This method will attempt to sign the device using our private cross
    /// signing key.
    ///
    /// This method will always fail if the device belongs to someone else, we
    /// can only sign our own devices.
    ///
    /// It can also fail if we don't have the private part of our self-signing
    /// key.
    ///
    /// The state of our private cross signing keys can be inspected using the
    /// [`Client::cross_signing_status()`] method.
    ///
    /// [`Client::cross_signing_status()`]: crate::Client::cross_signing_status
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
    /// # block_on(async {
    /// # let alice = UserId::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let device = client.get_device(&alice, "DEVICEID".into()).await?;
    ///
    /// if let Some(device) = device {
    ///     device.verify().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    pub async fn verify(&self) -> std::result::Result<(), ManualVerifyError> {
        let request = self.inner.verify().await?;
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Is the device considered to be verified.
    ///
    /// A device is considered to be verified, either if it's locally trusted,
    /// or if it's signed by the appropriate cross signing key.
    ///
    /// If the device belongs to our own userk, the device needs to be signed by
    /// our self-signing key and our own user identity needs to be verified.
    ///
    /// If the device belongs to some other user, the device needs to be signed
    /// by the users signing key and the user identity of the user needs to be
    /// verified.
    //// # Examples
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
    /// # block_on(async {
    /// # let alice = UserId::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     if user.verified() {
    ///         println!("User {} is verified", user.user_id().as_str());
    ///     } else {
    ///         println!("User {} is not verified", user.user_id().as_str());
    ///     }
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
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

/// The collection of all the [`Device`]s a user has.
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
