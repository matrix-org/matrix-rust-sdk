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

//! Interactive verification for E2EE capable users and devices in Matrix.
//!
//! The SDK supports interactive verification of devices and users, this module
//! contains types that model and support different verification flows.
//!
//! A verification flow usually starts its life as a [VerificationRequest], the
//! request can then be accepted, or it needs to be accepted by the other side
//! of the verification flow.
//!
//! Once both sides have agreed to pereform the verification, and the
//! [VerificationRequest::is_ready()] method returns true, the verification can
//! transition into one of the supported verification flows:
//!
//! * [SasVerification] - Interactive verification using a short authentication
//! string.
//! * [QrVerification] - Interactive verification using QR codes.

mod qrcode;
mod requests;
mod sas;

pub use qrcode::QrVerification;
pub use requests::VerificationRequest;
pub use sas::SasVerification;

/// An enum over the different verification types the SDK supports.
#[derive(Debug, Clone)]
pub enum Verification {
    /// The `m.sas.v1` verification variant.
    SasV1(SasVerification),
    /// The `m.qr_code.*.v1` verification variant.
    QrV1(QrVerification),
}

impl Verification {
    /// Try to deconstruct this verification enum into a SAS verification.
    pub fn sas(self) -> Option<SasVerification> {
        if let Verification::SasV1(sas) = self {
            Some(sas)
        } else {
            None
        }
    }

    /// Try to deconstruct this verification enum into a QR code verification.
    pub fn qr(self) -> Option<QrVerification> {
        if let Verification::QrV1(qr) = self {
            Some(qr)
        } else {
            None
        }
    }

    /// Has this verification finished.
    pub fn is_done(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_done(),
            Verification::QrV1(qr) => qr.is_done(),
        }
    }

    /// Has the verification been cancelled.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_cancelled(),
            Verification::QrV1(qr) => qr.is_cancelled(),
        }
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &ruma::UserId {
        match self {
            Verification::SasV1(v) => v.own_user_id(),
            Verification::QrV1(v) => v.own_user_id(),
        }
    }

    /// Get the user id of the other user participating in this verification
    /// flow.
    pub fn other_user_id(&self) -> &ruma::UserId {
        match self {
            Verification::SasV1(v) => v.inner.other_user_id(),
            Verification::QrV1(v) => v.inner.other_user_id(),
        }
    }

    /// Is this a verification that is veryfying one of our own devices.
    pub fn is_self_verification(&self) -> bool {
        match self {
            Verification::SasV1(v) => v.is_self_verification(),
            Verification::QrV1(v) => v.is_self_verification(),
        }
    }
}

impl From<SasVerification> for Verification {
    fn from(sas: SasVerification) -> Self {
        Self::SasV1(sas)
    }
}

impl From<QrVerification> for Verification {
    fn from(qr: QrVerification) -> Self {
        Self::QrV1(qr)
    }
}
