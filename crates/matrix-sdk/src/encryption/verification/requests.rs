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

use futures_util::{Stream, StreamExt};
use matrix_sdk_base::crypto::{CancelInfo, VerificationRequest as BaseVerificationRequest};
use ruma::{events::key::verification::VerificationMethod, OwnedDeviceId};

#[cfg(feature = "qrcode")]
use super::{QrVerification, QrVerificationData};
use super::{SasVerification, Verification};
use crate::{Client, Result};

/// An object controlling the interactive verification flow.
#[derive(Debug, Clone)]
pub struct VerificationRequest {
    pub(crate) inner: BaseVerificationRequest,
    pub(crate) client: Client,
}

/// An Enum describing the state the verification request is in.
#[derive(Debug, Clone)]
pub enum VerificationRequestState {
    /// The verification request has been newly created by us.
    Created {
        /// The verification methods supported by us.
        our_methods: Vec<VerificationMethod>,
    },
    /// The verification request was received from the other party.
    Requested {
        /// The verification methods supported by the sender.
        their_methods: Vec<VerificationMethod>,

        /// The device ID of the device that responded to the verification
        /// request.
        other_device_id: OwnedDeviceId,
    },
    /// The verification request is ready to start a verification flow.
    Ready {
        /// The verification methods supported by the other side.
        their_methods: Vec<VerificationMethod>,

        /// The verification methods supported by the us.
        our_methods: Vec<VerificationMethod>,

        /// The device ID of the device that responded to the verification
        /// request.
        other_device_id: OwnedDeviceId,
    },
    /// The verification request has transitioned into a concrete verification
    /// flow. For example it transitioned into the emoji based SAS
    /// verification.
    Transitioned {
        /// The concrete [`Verification`] object the verification request
        /// transitioned into.
        verification: Verification,
    },
    /// The verification flow that was started with this request has finished.
    Done,
    /// The verification process has been cancelled.
    Cancelled(CancelInfo),
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

    /// Is this a verification that is verifying one of our own devices.
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

    fn convert_state(
        client: Client,
        state: matrix_sdk_base::crypto::VerificationRequestState,
    ) -> VerificationRequestState {
        use matrix_sdk_base::crypto::VerificationRequestState::*;

        match state {
            Created { our_methods } => VerificationRequestState::Created { our_methods },
            Requested { their_methods, other_device_id } => {
                VerificationRequestState::Requested { their_methods, other_device_id }
            }
            Ready { their_methods, our_methods, other_device_id } => {
                VerificationRequestState::Ready { their_methods, our_methods, other_device_id }
            }
            Transitioned { verification } => VerificationRequestState::Transitioned {
                verification: match verification {
                    matrix_sdk_base::crypto::Verification::SasV1(s) => {
                        Verification::SasV1(SasVerification { inner: s, client })
                    }
                    #[cfg(feature = "qrcode")]
                    matrix_sdk_base::crypto::Verification::QrV1(q) => {
                        Verification::QrV1(QrVerification { inner: q, client })
                    }
                    _ => unreachable!("We only support QR code and SAS verification"),
                },
            },
            Done => VerificationRequestState::Done,
            Cancelled(c) => VerificationRequestState::Cancelled(c),
        }
    }

    /// Listen for changes in the verification request.
    ///
    /// The changes are presented as a stream of [`VerificationRequestState`]
    /// values.
    pub fn changes(&self) -> impl Stream<Item = VerificationRequestState> {
        let client = self.client.to_owned();

        self.inner.changes().map(move |s| Self::convert_state(client.to_owned(), s))
    }

    /// Get the current state the verification request is in.
    ///
    /// To listen to changes to the [`VerificationRequestState`] use the
    /// [`VerificationRequest::changes`] method.
    pub fn state(&self) -> VerificationRequestState {
        Self::convert_state(self.client.to_owned(), self.inner.state())
    }
}
