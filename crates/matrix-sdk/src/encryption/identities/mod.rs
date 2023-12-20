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

//! Cryptographic identities used in Matrix.
//!
//! There are two types of cryptographic identities in Matrix.
//!
//! 1. Devices, which are backed by [device keys], they represent each
//! individual log in by an E2EE capable Matrix client. We represent devices
//! using the [`Device`] struct.
//!
//! 2. User identities, which are backed by [cross signing keys]. The user
//! identity represent a unique E2EE capable identity of any given user. This
//! identity is generally created and uploaded to the server by the first E2EE
//! capable client the user logs in with. We represent user identities using the
//! [`UserIdentity`] struct.
//!
//! A [`Device`] or an [`UserIdentity`] can be used to inspect the public keys
//! of the device/identity, or it can be used to initiate a interactive
//! verification flow. They can also be manually marked as verified.
//!
//! # Examples
//!
//! Verifying a device is pretty straightforward:
//!
//! ```no_run
//! # use matrix_sdk::{Client, ruma::{device_id, user_id}};
//! # use url::Url;
//! # let alice = user_id!("@alice:example.org");
//! # let homeserver = Url::parse("http://example.com").unwrap();
//! # async {
//! # let client = Client::new(homeserver).await.unwrap();
//! let device =
//!     client.encryption().get_device(alice, device_id!("DEVICEID")).await?;
//!
//! if let Some(device) = device {
//!     // Let's request the device to be verified.
//!     let verification = device.request_verification().await?;
//!
//!     // Actually this is taking too long.
//!     verification.cancel().await?;
//!
//!     // Let's just mark it as verified.
//!     device.verify().await?;
//! }
//! # anyhow::Ok(()) };
//! ```
//!
//! Verifying a user identity works largely the same:
//!
//! ```no_run
//! # use matrix_sdk::{Client, ruma::user_id};
//! # use url::Url;
//! # let alice = user_id!("@alice:example.org");
//! # let homeserver = Url::parse("http://example.com").unwrap();
//! # async {
//! # let client = Client::new(homeserver).await.unwrap();
//! let user = client.encryption().get_user_identity(alice).await?;
//!
//! if let Some(user) = user {
//!     // Let's request the user to be verified.
//!     let verification = user.request_verification().await?;
//!
//!     // Actually this is taking too long.
//!     verification.cancel().await?;
//!
//!     // Let's just mark it as verified.
//!     user.verify().await?;
//! }
//! # anyhow::Ok(()) };
//! ```
//!
//! [cross signing keys]: https://spec.matrix.org/unstable/client-server-api/#cross-signing
//! [device keys]: https://spec.matrix.org/unstable/client-server-api/#device-keys

mod devices;
mod users;

pub use devices::{Device, DeviceUpdates, UserDevices};
pub use matrix_sdk_base::crypto::types::MasterPubkey;
pub use users::{IdentityUpdates, UserIdentity};

/// Error for the manual verification step, when we manually sign users or
/// devices.
#[derive(thiserror::Error, Debug)]
pub enum ManualVerifyError {
    /// Error that happens when we try to upload the user or device signature.
    #[error(transparent)]
    Http(#[from] crate::HttpError),
    /// Error that happens when we try to sign the user or device.
    #[error(transparent)]
    Signature(#[from] matrix_sdk_base::crypto::SignatureError),
}

/// Error when requesting a verification.
#[derive(thiserror::Error, Debug)]
pub enum RequestVerificationError {
    /// An ordinary error coming from the SDK, i.e. when we fail to send out a
    /// HTTP request or if there's an error with the storage layer.
    #[error(transparent)]
    Sdk(#[from] crate::Error),
    /// Verifying other users requires having a DM open with them, this error
    /// signals that we didn't have a DM and that we failed to create one.
    #[error("Couldn't create a DM with user {0} where the verification should take place")]
    RoomCreation(ruma::OwnedUserId),
}
