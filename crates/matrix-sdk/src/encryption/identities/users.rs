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

use std::{result::Result, sync::Arc};

use matrix_sdk_base::{
    crypto::{
        MasterPubkey, OwnUserIdentity as InnerOwnUserIdentity, UserIdentity as InnerUserIdentity,
    },
    locks::RwLock,
};
use ruma::{
    events::{
        key::verification::VerificationMethod,
        room::message::{MessageEventContent, MessageType},
    },
    UserId,
};

use super::{ManualVerifyError, RequestVerificationError};
use crate::{encryption::verification::VerificationRequest, room::Joined, Client};

/// A struct representing a E2EE capable identity of a user.
///
/// The identity is backed by public [cross signing] keys that users upload.
///
/// [cross signing]: https://spec.matrix.org/unstable/client-server-api/#cross-signing
#[derive(Debug, Clone)]
pub struct UserIdentity {
    inner: UserIdentities,
}

impl UserIdentity {
    pub(crate) fn new_own(client: Client, identity: InnerOwnUserIdentity) -> Self {
        let identity = OwnUserIdentity { inner: identity, client };

        Self { inner: identity.into() }
    }

    pub(crate) fn new(client: Client, identity: InnerUserIdentity, room: Option<Joined>) -> Self {
        let identity = OtherUserIdentity {
            inner: identity,
            client,
            direct_message_room: RwLock::new(room).into(),
        };

        Self { inner: identity.into() }
    }

    /// The ID of the user this E2EE identity belongs to.
    pub fn user_id(&self) -> &UserId {
        match &self.inner {
            UserIdentities::Own(i) => i.inner.user_id(),
            UserIdentities::Other(i) => i.inner.user_id(),
        }
    }

    /// Request an interacitve verification with this `UserIdentity`.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// This will send out a `m.key.verification.request` event to all the E2EE
    /// capable devices we have if we're requesting verification with our own
    /// user identity or will send out the event to a DM we share with the user.
    ///
    /// If we don't share a DM with this user one will be created before the
    /// event gets sent out.
    ///
    /// The default methods that are supported are `m.sas.v1` and
    /// `m.qr_code.show.v1`, if this isn't desirable the
    /// [`request_verification_with_methods()`] method can be used to override
    /// this. `m.qr_code.show.v1` is only available if the `qrcode` feature is
    /// enabled, which it is by default.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # futures::executor::block_on(async {
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     let verification = user.request_verification().await?;
    /// }
    ///
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    ///
    /// [`request_verification_with_methods()`]:
    /// #method.request_verification_with_methods
    pub async fn request_verification(
        &self,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        match &self.inner {
            UserIdentities::Own(i) => i.request_verification(None).await,
            UserIdentities::Other(i) => i.request_verification(None).await,
        }
    }

    /// Request an interacitve verification with this `UserIdentity`.
    ///
    /// Returns a [`VerificationRequest`] object that can be used to control the
    /// verification flow.
    ///
    /// This methods behaves the same way as [`request_verification()`],
    /// but the advertised verification methods can be manually selected.
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
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// // We don't want to support showing a QR code, we only support SAS
    /// // verification
    /// let methods = vec![VerificationMethod::SasV1];
    ///
    /// if let Some(user) = user {
    ///     let verification = user.request_verification_with_methods(methods).await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    ///
    /// [`request_verification()`]: #method.request_verification
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        assert!(!methods.is_empty(), "The list of verification methods can't be non-empty");

        match &self.inner {
            UserIdentities::Own(i) => i.request_verification(Some(methods)).await,
            UserIdentities::Other(i) => i.request_verification(Some(methods)).await,
        }
    }

    /// Manually verify this [`UserIdentity`].
    ///
    /// This method will attempt to sign the user identity using our private
    /// cross signing key. Verifying can fail if we don't have the private
    /// part of our user-signing key.
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
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     user.verify().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    pub async fn verify(&self) -> Result<(), ManualVerifyError> {
        match &self.inner {
            UserIdentities::Own(i) => i.verify().await,
            UserIdentities::Other(i) => i.verify().await,
        }
    }

    /// Is the user identity considered to be verified.
    ///
    /// A user identity is considered to be verified if it has been signed by
    /// our user-signing key, if the identity belongs to another user, or if we
    /// locally marked it as verified, if the user identity belongs to us.
    ///
    /// If the identity belongs to another user, our own user identity needs to
    /// be verified as well for the identity to be considered to be verified.
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
        match &self.inner {
            UserIdentities::Own(i) => i.inner.is_verified(),
            UserIdentities::Other(i) => i.inner.verified(),
        }
    }

    /// Get the public part of the master key of this user identity.
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
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     // Let's verify the user after we confirm that the master key
    ///     // matches what we expect, for this we fetch the first public key we
    ///     // can find, there's currently only a single key allowed so this is
    ///     // fine.
    ///     if user.master_key().get_first_key() == Some("MyMasterKey") {
    ///         println!(
    ///             "Master keys match for user {}, marking the user as verified",
    ///             user.user_id().as_str(),
    ///         );
    ///         user.verify().await?;
    ///     } else {
    ///         println!("Master keys don't match for user {}", user.user_id().as_str());
    ///     }
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    pub fn master_key(&self) -> &MasterPubkey {
        match &self.inner {
            UserIdentities::Own(i) => i.inner.master_key(),
            UserIdentities::Other(i) => i.inner.master_key(),
        }
    }
}

#[derive(Debug, Clone)]
enum UserIdentities {
    Own(OwnUserIdentity),
    Other(OtherUserIdentity),
}

impl From<OwnUserIdentity> for UserIdentities {
    fn from(i: OwnUserIdentity) -> Self {
        Self::Own(i)
    }
}

impl From<OtherUserIdentity> for UserIdentities {
    fn from(i: OtherUserIdentity) -> Self {
        Self::Other(i)
    }
}

#[derive(Debug, Clone)]
struct OwnUserIdentity {
    pub(crate) inner: InnerOwnUserIdentity,
    pub(crate) client: Client,
}

#[derive(Debug, Clone)]
struct OtherUserIdentity {
    pub(crate) inner: InnerUserIdentity,
    pub(crate) client: Client,
    pub(crate) direct_message_room: Arc<RwLock<Option<Joined>>>,
}

impl OwnUserIdentity {
    async fn request_verification(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        let (verification, request) = if let Some(methods) = methods {
            self.inner
                .request_verification_with_methods(methods)
                .await
                .map_err(crate::Error::from)?
        } else {
            self.inner.request_verification().await.map_err(crate::Error::from)?
        };

        self.client.send_verification_request(request).await?;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    async fn verify(&self) -> Result<(), ManualVerifyError> {
        let request = self.inner.verify().await?;
        self.client.send(request, None).await?;

        Ok(())
    }
}

impl OtherUserIdentity {
    async fn request_verification(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Result<VerificationRequest, RequestVerificationError> {
        let content = self.inner.verification_request_content(methods.clone()).await;

        let room = if let Some(room) = self.direct_message_room.read().await.as_ref() {
            room.clone()
        } else if let Some(room) =
            self.client.create_dm_room(self.inner.user_id().to_owned()).await?
        {
            room
        } else {
            return Err(RequestVerificationError::RoomCreation(self.inner.user_id().to_owned()));
        };

        let response = room
            .send(MessageEventContent::new(MessageType::VerificationRequest(content)), None)
            .await?;

        let verification =
            self.inner.request_verification(room.room_id(), &response.event_id, methods).await;

        Ok(VerificationRequest { inner: verification, client: self.client.clone() })
    }

    async fn verify(&self) -> Result<(), ManualVerifyError> {
        let request = self.inner.verify().await?;
        self.client.send(request, None).await?;

        Ok(())
    }
}
