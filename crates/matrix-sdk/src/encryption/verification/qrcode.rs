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

use futures_core::Stream;
use matrix_sdk_base::crypto::{
    CancelInfo, DeviceData, QrVerification as BaseQrVerification, QrVerificationState,
    matrix_sdk_qrcode::{EncodingError, qrcode::QrCode},
};
use ruma::{RoomId, UserId};

use crate::{Client, Result};

/// An object controlling QR code style key verification flows.
#[derive(Debug, Clone)]
pub struct QrVerification {
    pub(crate) inner: Box<BaseQrVerification>,
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

    /// Get the other user's device that we're verifying.
    pub fn other_device(&self) -> &DeviceData {
        self.inner.other_device()
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

    /// Listen for changes in the QR code verification process.
    ///
    /// The changes are presented as a stream of [`QrVerificationState`] values.
    ///
    /// This method can be used to react to changes in the state of the
    /// verification process, or rather the method can be used to handle
    /// each step of the verification process.
    ///
    /// # Flowchart
    ///
    /// The flow of the verification process is pictured below. Please note
    /// that the process can be cancelled at each step of the process.
    /// Either side can cancel the process.
    ///
    /// ```text
    ///                ┌───────┐
    ///                │Started│
    ///                └───┬───┘
    ///                    │
    ///                    │
    ///             ┌──────⌄─────┐
    ///             │Reciprocated│
    ///             └──────┬─────┘
    ///                    │
    ///                ┌───⌄───┐
    ///                │Scanned│
    ///                └───┬───┘
    ///                    │
    ///          __________⌄_________
    ///         ╱                    ╲       ┌─────────┐
    ///        ╱   Was the QR Code    ╲______│Cancelled│
    ///        ╲ successfully scanned ╱ no   └─────────┘
    ///         ╲____________________╱
    ///                    │yes
    ///                    │
    ///               ┌────⌄────┐
    ///               │Confirmed│
    ///               └────┬────┘
    ///                    │
    ///                ┌───⌄───┐
    ///                │  Done │
    ///                └───────┘
    /// ```
    /// # Examples
    ///
    /// ```no_run
    /// use futures_util::{Stream, StreamExt};
    /// use matrix_sdk::encryption::verification::{
    ///     QrVerification, QrVerificationState,
    /// };
    ///
    /// # async {
    /// # let qr: QrVerification = unimplemented!();
    /// # let user_confirmed = false;
    /// let mut stream = qr.changes();
    ///
    /// while let Some(state) = stream.next().await {
    ///     match state {
    ///         QrVerificationState::Scanned => {
    ///             println!("Was the QR code successfully scanned?");
    ///
    ///             // Ask the user to confirm or cancel here.
    ///             if user_confirmed {
    ///                 qr.confirm().await?;
    ///             } else {
    ///                 qr.cancel().await?;
    ///             }
    ///         }
    ///         QrVerificationState::Done { .. } => {
    ///             let device = qr.other_device();
    ///
    ///             println!(
    ///                 "Successfully verified device {} {} {:?}",
    ///                 device.user_id(),
    ///                 device.device_id(),
    ///                 device.local_trust_state()
    ///             );
    ///
    ///             break;
    ///         }
    ///         QrVerificationState::Cancelled(cancel_info) => {
    ///             println!(
    ///                 "The verification has been cancelled, reason: {}",
    ///                 cancel_info.reason()
    ///             );
    ///             break;
    ///         }
    ///         QrVerificationState::Started
    ///         | QrVerificationState::Reciprocated
    ///         | QrVerificationState::Confirmed => (),
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn changes(&self) -> impl Stream<Item = QrVerificationState> + use<> {
        self.inner.changes()
    }

    /// Get the current state the verification process is in.
    ///
    /// To listen to changes to the [`QrVerificationState`] use the
    /// [`QrVerification::changes`] method.
    pub fn state(&self) -> QrVerificationState {
        self.inner.state()
    }

    /// Get the room ID, if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<&RoomId> {
        self.inner.room_id()
    }
}
