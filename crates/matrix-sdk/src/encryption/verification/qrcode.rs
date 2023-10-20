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

use matrix_sdk_base::crypto::{
    matrix_sdk_qrcode::{qrcode::QrCode, EncodingError},
    CancelInfo, QrVerification as BaseQrVerification,
};
use ruma::UserId;

use crate::{Client, Result};

/// An object controlling QR code style key verification flows.
#[derive(Debug, Clone)]
pub struct QrVerification {
    pub(crate) inner: BaseQrVerification,
    pub(crate) client: Client,
}

impl QrVerification {
    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        self.inner.user_id()
    }

    /// Is this a verification that is verifying one of our own devices.
    pub fn is_self_verification(&self) -> bool {
        self.inner.is_self_verification()
    }

    /// Has this verification finished.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Whether the QrCode was scanned by the other device.
    pub fn has_been_scanned(&self) -> bool {
        self.inner.has_been_scanned()
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Get info about the cancellation if the verification flow has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info()
    }

    /// Get the user id of the other user participating in this verification
    /// flow.
    pub fn other_user_id(&self) -> &UserId {
        self.inner.other_user_id()
    }

    /// Has the verification been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Generate a QR code object that is representing this verification flow.
    ///
    /// The `QrCode` can then be rendered as an image or as an unicode string.
    ///
    /// The [`to_bytes()`](#method.to_bytes) method can be used to instead
    /// output the raw bytes that should be encoded as a QR code.
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        self.inner.to_qr_code()
    }

    /// Generate a the raw bytes that should be encoded as a QR code is
    /// representing this verification flow.
    ///
    /// The [`to_qr_code()`](#method.to_qr_code) method can be used to instead
    /// output a `QrCode` object that can be rendered.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        self.inner.to_bytes()
    }

    /// Confirm that the other side has scanned our QR code.
    pub async fn confirm(&self) -> Result<()> {
        if let Some(request) = self.inner.confirm_scanning() {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }

    /// Abort the verification flow and notify the other side that we did so.
    pub async fn cancel(&self) -> Result<()> {
        if let Some(request) = self.inner.cancel() {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }
}
