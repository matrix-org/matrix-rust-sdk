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

use std::collections::BTreeMap;

use matrix_sdk_base::{
    crypto::{types::MasterPubkey, UserIdentities as CryptoUserIdentities},
    RoomMemberships,
};
use ruma::{
    events::{
        key::verification::VerificationMethod,
        room::message::{MessageType, RoomMessageEventContent},
    },
    OwnedUserId, UserId,
};

use super::{ManualVerifyError, RequestVerificationError};
use crate::{encryption::verification::VerificationRequest, Client};

/// Updates about [`UserIdentity`]s which got received over the `/keys/query`
/// endpoint.
#[derive(Clone, Debug, Default)]
pub struct IdentityUpdates {
    /// The list of newly discovered user identities .
    ///
    /// A identity being in this list does not necessarily mean that the
    /// identity was just created, it just means that it's the first time
    /// we're seeing this identity.
    pub new: BTreeMap<OwnedUserId, UserIdentity>,
    /// The list of changed identities.
    pub changed: BTreeMap<OwnedUserId, UserIdentity>,
}

impl IdentityUpdates {
    pub(crate) fn new(
        client: Client,
        updates: matrix_sdk_base::crypto::store::IdentityUpdates,
    ) -> Self {
        let new = updates
            .new
            .into_iter()
            .map(|(user_id, identity)| (user_id, UserIdentity::new(client.to_owned(), identity)))
            .collect();

        let changed = updates
            .changed
            .into_iter()
            .map(|(user_id, identity)| (user_id, UserIdentity::new(client.to_owned(), identity)))
            .collect();

        Self { new, changed }
    }
}

/// A struct representing a E2EE capable identity of a user.
///
/// The identity is backed by public [cross signing] keys that users upload. If
/// our own user doesn't yet have such an identity, a new one can be created and
/// uploaded to the server using [`Encryption::bootstrap_cross_signing()`]. The
/// user identity can be also reset using the same method.
///
/// The user identity consists of three separate `Ed25519` keypairs:
///
/// ```text
///           ┌──────────────────────────────────────────────────────┐
///           │                    User Identity                     │
///           ├────────────────┬──────────────────┬──────────────────┤
///           │   Master Key   │ Self-signing Key │ User-signing key │
///           └────────────────┴──────────────────┴──────────────────┘
/// ```
///
/// The identity consists of a Master key and two sub-keys, the Self-signing key
/// and the User-signing key.
///
/// Each key has a separate role:
/// * Master key, signs only the sub-keys, can be used as a fingerprint of the
/// identity.
/// * Self-signing key, signs devices belonging to the user that owns this
/// identity.
/// * User-signing key, signs Master keys belonging to other users.
///
/// The User-signing key and its signatures of other user's Master keys are
/// hidden from us by the homeserver. This is done to preserve privacy and not
/// let us know whom the user verified.
///
/// [cross signing]: https://spec.matrix.org/unstable/client-server-api/#cross-signing
/// [`Encryption::bootstrap_cross_signing()`]: crate::encryption::Encryption::bootstrap_cross_signing
#[derive(Debug, Clone)]
pub struct UserIdentity {
    inner: UserIdentities,
}

impl UserIdentity {
    pub(crate) fn new(client: Client, identity: CryptoUserIdentities) -> Self {
        let inner = UserIdentities { client, identity };
        Self { inner }
    }

    /// The ID of the user this identity belongs to.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("This user identity belongs to {}", user.user_id());
    /// }
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub fn user_id(&self) -> &UserId {
        match &self.inner.identity {
            CryptoUserIdentities::Own(identity) => identity.user_id(),
            CryptoUserIdentities::Other(identity) => identity.user_id(),
        }
    }

    /// Request an interactive verification with this `UserIdentity`.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// This will send out a `m.key.verification.request` event. Who such an
    /// event will be sent to depends on if we're verifying our own identity or
    /// someone else's:
    ///
    /// * Our own identity - All our E2EE capable devices will receive the event
    /// over to-device messaging.
    /// * Someone else's identity - The event will be sent to a DM room we share
    /// with the user, if we don't share a DM with the user, one will be
    /// created.
    ///
    /// The default methods that are supported are:
    ///
    /// * `m.sas.v1` - Short auth string, or emoji based verification
    /// * `m.qr_code.show.v1` - QR code based verification
    ///
    /// [`request_verification_with_methods()`] method can be
    /// used to override this. The `m.qr_code.show.v1` method is only available
    /// if the `qrcode` feature is enabled, which it is by default.
    ///
    /// Check out the [`verification`] module for more info on how to handle
    /// interactive verifications.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     let verification = user.request_verification().await?;
    /// }
    ///
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`request_verification_with_methods()`]:
    /// #method.request_verification_with_methods
    /// [`verification`]: crate::encryption::verification
    pub async fn request_verification(
        &self,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        self.inner.request_verification(None).await
    }

    /// Request an interactive verification with this `UserIdentity` using the
    /// selected methods.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// This methods behaves the same way as [`request_verification()`],
    /// but the advertised verification methods can be manually selected.
    ///
    /// Check out the [`verification`] module for more info on how to handle
    /// interactive verifications.
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
    /// # use matrix_sdk::{
    /// #    Client,
    /// #    ruma::{
    /// #        user_id,
    /// #        events::key::verification::VerificationMethod,
    /// #    }
    /// # };
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// // We don't want to support showing a QR code, we only support SAS
    /// // verification
    /// let methods = vec![VerificationMethod::SasV1];
    ///
    /// if let Some(user) = user {
    ///     let verification =
    ///         user.request_verification_with_methods(methods).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`request_verification()`]: #method.request_verification
    /// [`verification`]: crate::encryption::verification
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        assert!(!methods.is_empty(), "The list of verification methods can't be non-empty");

        self.inner.request_verification(Some(methods)).await
    }

    /// Manually verify this [`UserIdentity`].
    ///
    /// This method will do different things depending on if the user identity
    /// belongs to us, or if the user identity belongs to someone else. Users
    /// that chose to manually verify a user identity should make sure that the
    /// Master key does match to to the `Ed25519` they expect.
    ///
    /// The Master key can be inspected using the [`UserIdentity::master_key()`]
    /// method.
    ///
    /// ### Manually verifying other users
    ///
    /// This method will attempt to sign the user identity using our private
    /// parts of the cross signing keys. The method will attempt to sign the
    /// Master key of the user using our own User-signing key. This will of
    /// course fail if the private part of the User-signing key isn't available.
    ///
    /// The availability of the User-signing key can be checked using the
    /// [`Encryption::cross_signing_status()`] method.
    ///
    /// ### Manually verifying our own user
    ///
    /// On the other hand, if the user identity belongs to us, it will be
    /// marked as verified using a local flag, our own device will also sign the
    /// Master key. Manually verifying our own user identity can't fail.
    ///
    /// ### Problems of manual verification
    ///
    /// Manual verification may be more convenient to use, i.e. both users need
    /// to be online and available to interactively verify each other. Despite
    /// the convenience, interactive verifications should be generally
    /// preferred. Manually verifying a user won't notify the other user, the
    /// one being verified, that they should also verify us. This means that
    /// user `A` will consider user `B` to be verified, but not the other way
    /// around.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    Client,
    /// #    ruma::{
    /// #        user_id,
    /// #        events::key::verification::VerificationMethod,
    /// #    }
    /// # };
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     user.verify().await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    /// [`Encryption::cross_signing_status()`]: crate::encryption::Encryption::cross_signing_status
    pub async fn verify(&self) -> Result<(), ManualVerifyError> {
        self.inner.verify().await
    }

    /// Is the user identity considered to be verified.
    ///
    /// A user identity is considered to be verified if:
    ///
    /// * It has been signed by our User-signing key, if the identity belongs
    /// to another user
    /// * If it has been locally marked as verified, if the user identity
    /// belongs to us.
    ///
    /// If the identity belongs to another user, our own user identity needs to
    /// be verified as well for the identity to be considered to be verified.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    Client,
    /// #    ruma::{
    /// #        user_id,
    /// #        events::key::verification::VerificationMethod,
    /// #    }
    /// # };
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     if user.is_verified() {
    ///         println!("User {} is verified", user.user_id());
    ///     } else {
    ///         println!("User {} is not verified", user.user_id());
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn is_verified(&self) -> bool {
        match &self.inner.identity {
            CryptoUserIdentities::Own(identity) => identity.is_verified(),
            CryptoUserIdentities::Other(identity) => identity.is_verified(),
        }
    }

    /// Get the public part of the Master key of this user identity.
    ///
    /// The public part of the Master key is usually used to uniquely identify
    /// the identity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    Client,
    /// #    ruma::{
    /// #        user_id,
    /// #        events::key::verification::VerificationMethod,
    /// #    }
    /// # };
    /// # use url::Url;
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     // Let's verify the user after we confirm that the master key
    ///     // matches what we expect, for this we fetch the first public key we
    ///     // can find, there's currently only a single key allowed so this is
    ///     // fine.
    ///     if user.master_key().get_first_key().map(|k| k.to_base64())
    ///         == Some("MyMasterKey".to_string())
    ///     {
    ///         println!(
    ///             "Master keys match for user {}, marking the user as verified",
    ///             user.user_id(),
    ///         );
    ///         user.verify().await?;
    ///     } else {
    ///         println!("Master keys don't match for user {}", user.user_id());
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn master_key(&self) -> &MasterPubkey {
        match &self.inner.identity {
            CryptoUserIdentities::Own(identity) => identity.master_key(),
            CryptoUserIdentities::Other(identity) => identity.master_key(),
        }
    }
}

#[derive(Debug, Clone)]
struct UserIdentities {
    client: Client,
    identity: CryptoUserIdentities,
}

impl UserIdentities {
    async fn request_verification(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        match &self.identity {
            CryptoUserIdentities::Own(identity) => {
                let (verification, request) = if let Some(methods) = methods {
                    identity
                        .request_verification_with_methods(methods)
                        .await
                        .map_err(crate::Error::from)?
                } else {
                    identity.request_verification().await.map_err(crate::Error::from)?
                };

                self.client.send_verification_request(request).await?;

                Ok(VerificationRequest { inner: verification, client: self.client.clone() })
            }
            CryptoUserIdentities::Other(i) => {
                let content = i.verification_request_content(methods.clone());

                let room = if let Some(room) = self.client.get_dm_room(i.user_id()) {
                    // Make sure that the user, to be verified, is still in the room
                    if !room
                        .members(RoomMemberships::ACTIVE)
                        .await?
                        .iter()
                        .any(|member| member.user_id() == i.user_id())
                    {
                        room.invite_user_by_id(i.user_id()).await?;
                    }
                    room.clone()
                } else {
                    self.client.create_dm(i.user_id()).await?
                };

                let response = room
                    .send(RoomMessageEventContent::new(MessageType::VerificationRequest(content)))
                    .await?;

                let verification =
                    i.request_verification(room.room_id(), &response.event_id, methods);

                Ok(VerificationRequest { inner: verification, client: self.client.clone() })
            }
        }
    }

    async fn verify(&self) -> Result<(), ManualVerifyError> {
        let request = match &self.identity {
            CryptoUserIdentities::Own(identity) => identity.verify().await?,
            CryptoUserIdentities::Other(identity) => identity.verify().await?,
        };
        self.client.send(request, None).await?;
        Ok(())
    }
}
