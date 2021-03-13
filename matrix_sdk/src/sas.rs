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

use matrix_sdk_base::crypto::{
    AcceptSettings, OutgoingVerificationRequest, ReadOnlyDevice, Sas as BaseSas,
};

use crate::{error::Result, Client};

/// An object controling the interactive verification flow.
#[derive(Debug, Clone)]
pub struct Sas {
    pub(crate) inner: BaseSas,
    pub(crate) client: Client,
}

impl Sas {
    /// Accept the interactive verification flow.
    pub async fn accept(&self) -> Result<()> {
        self.accept_with_settings(Default::default()).await
    }

    /// Accept the interactive verification flow with specific settings.
    ///
    /// # Arguments
    ///
    /// * `settings` - specific customizations to the verification flow.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use matrix_sdk::Sas;
    /// use matrix_sdk_base::crypto::AcceptSettings;
    /// use matrix_sdk::events::key::verification::ShortAuthenticationString;
    ///
    /// let only_decimal = AcceptSettings::with_allowed_methods(
    ///     vec![ShortAuthenticationString::Decimal]
    /// );
    /// sas.accept_with_settings(only_decimal);
    /// ```
    pub async fn accept_with_settings(&self, settings: AcceptSettings) -> Result<()> {
        if let Some(req) = self.inner.accept_with_settings(settings) {
            match req {
                OutgoingVerificationRequest::ToDevice(r) => {
                    self.client.send_to_device(&r).await?;
                }
                OutgoingVerificationRequest::InRoom(_) => todo!(),
            }
        }
        Ok(())
    }

    /// Confirm that the short auth strings match on both sides.
    pub async fn confirm(&self) -> Result<()> {
        let (request, signature) = self.inner.confirm().await?;

        match request {
            Some(OutgoingVerificationRequest::InRoom(r)) => {
                self.client.room_send_helper(&r).await?;
            }

            Some(OutgoingVerificationRequest::ToDevice(r)) => {
                self.client.send_to_device(&r).await?;
            }

            None => (),
        }

        if let Some(s) = signature {
            self.client.send(s, None).await?;
        }

        Ok(())
    }

    /// Cancel the interactive verification flow.
    pub async fn cancel(&self) -> Result<()> {
        if let Some(request) = self.inner.cancel() {
            match request {
                OutgoingVerificationRequest::ToDevice(r) => {
                    self.client.send_to_device(&r).await?;
                }
                OutgoingVerificationRequest::InRoom(r) => {
                    self.client.room_send_helper(&r).await?;
                }
            }
        }

        Ok(())
    }

    /// Get the emoji version of the short auth string.
    pub fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        self.inner.emoji()
    }

    /// Get the decimal version of the short auth string.
    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        self.inner.decimals()
    }

    /// Is the verification process done.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Is the verification process canceled.
    pub fn is_canceled(&self) -> bool {
        self.inner.is_canceled()
    }

    /// Get the other users device that we're veryfying.
    pub fn other_device(&self) -> ReadOnlyDevice {
        self.inner.other_device()
    }
}
