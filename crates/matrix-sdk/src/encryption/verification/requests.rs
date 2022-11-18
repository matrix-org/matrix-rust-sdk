// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_base::crypto::{CancelInfo, VerificationRequest as BaseVerificationRequest};
use ruma::events::key::verification::VerificationMethod;

use super::SasVerification;
#[cfg(feature = "qrcode")]
use super::{QrVerification, QrVerificationData};
use crate::{Client, Result};

/// An object controlling the interactive verification flow.
#[derive(Debug, Clone)]
pub struct VerificationRequest {
    pub(crate) inner: BaseVerificationRequest,
    pub(crate) client: Client,
}

impl VerificationRequest {
    /// Has this verification finished.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Has the verification been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Get the transaction id of this verification request
    pub fn flow_id(&self) -> &str {
        self.inner.flow_id().as_str()
    }

    /// Get info about the cancellation if the verification request has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info()
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &ruma::UserId {
        self.inner.own_user_id()
    }

    /// Has the verification request been answered by another device.
    pub fn is_passive(&self) -> bool {
        self.inner.is_passive()
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Get the user id of the other user participating in this verification
    /// flow.
    pub fn other_user_id(&self) -> &ruma::UserId {
        self.inner.other_user()
    }

    /// Is this a verification that is veryfying one of our own devices.
    pub fn is_self_verification(&self) -> bool {
        self.inner.is_self_verification()
    }

    /// Get the supported verification methods of the other side.
    ///
    /// Will be present only if the other side requested the verification or if
    /// we're in the ready state.
    pub fn their_supported_methods(&self) -> Option<Vec<VerificationMethod>> {
        self.inner.their_supported_methods()
    }

    /// Accept the verification request.
    ///
    /// This method will accept the request and signal by default that it
    /// supports the `m.sas.v1`, the `m.qr_code.show.v1`, and `m.reciprocate.v1`
    /// method. If the `qrcode` feature is disabled it will only signal that it
    /// supports the `m.sas.v1` method.
    ///
    /// If QR code scanning should be supported or QR code showing shouldn't be
    /// supported the [`accept_with_methods()`] method should be used instead.
    ///
    /// [`accept_with_methods()`]: #method.accept_with_methods
    pub async fn accept(&self) -> Result<()> {
        if let Some(request) = self.inner.accept() {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }

    /// Accept the verification request signaling that our client supports the
    /// given verification methods.
    ///
    /// # Arguments
    ///
    /// * `methods` - The methods that we should advertise as supported by us.
    pub async fn accept_with_methods(&self, methods: Vec<VerificationMethod>) -> Result<()> {
        if let Some(request) = self.inner.accept_with_methods(methods) {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }

    /// Generate a QR code
    #[cfg(feature = "qrcode")]
    pub async fn generate_qr_code(&self) -> Result<Option<QrVerification>> {
        Ok(self
            .inner
            .generate_qr_code()
            .await?
            .map(|qr| QrVerification { inner: qr, client: self.client.clone() }))
    }

    /// Start a QR code verification by providing a scanned QR code for this
    /// verification flow.
    ///
    /// Returns an `Error` if the QR code isn't valid or sending a reciprocate
    /// event to the other side fails, `None` if the verification request
    /// isn't in the ready state or we don't support QR code verification,
    /// otherwise a newly created `QrVerification` object which will be used
    /// for the remainder of the verification flow.
    #[cfg(feature = "qrcode")]
    pub async fn scan_qr_code(&self, data: QrVerificationData) -> Result<Option<QrVerification>> {
        let Some(qr) = self.inner.scan_qr_code(data).await? else { return Ok(None) };
        if let Some(request) = qr.reciprocate() {
            self.client.send_verification_request(request).await?;
        }

        Ok(Some(QrVerification { inner: qr, client: self.client.clone() }))
    }

    /// Transition from this verification request into a SAS verification flow.
    pub async fn start_sas(&self) -> Result<Option<SasVerification>> {
        let Some((sas, request)) = self.inner.start_sas().await? else { return Ok(None) };
        self.client.send_verification_request(request).await?;

        Ok(Some(SasVerification { inner: sas, client: self.client.clone() }))
    }

    /// Cancel the verification request
    pub async fn cancel(&self) -> Result<()> {
        if let Some(request) = self.inner.cancel() {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }
}
