// Copyright 2021 The Matrix.org Foundation C.I.C.
// Copyright 2021 Damir Jelić
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

#![doc = include_str!("../docs/encryption.md")]
#![cfg_attr(target_arch = "wasm32", allow(unused_imports))]

pub mod identities;
pub mod verification;
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    iter,
    path::PathBuf,
};

use futures_util::stream::{self, StreamExt};
use matrix_sdk_base::crypto::{
    store::CryptoStoreError, CrossSigningStatus, OutgoingRequest, RoomMessageRequest,
    ToDeviceRequest,
};
pub use matrix_sdk_base::crypto::{LocalTrust, MediaEncryptionInfo, RoomKeyImportResult};
use matrix_sdk_common::instant::Duration;
#[cfg(feature = "e2e-encryption")]
use ruma::OwnedDeviceId;
use ruma::{
    api::client::{
        backup::add_backup_keys::v3::Response as KeysBackupResponse,
        keys::{
            get_keys, upload_keys, upload_signing_keys::v3::Request as UploadSigningKeysRequest,
        },
        message::send_message_event,
        to_device::send_event_to_device::v3::{
            Request as RumaToDeviceRequest, Response as ToDeviceResponse,
        },
        uiaa::AuthData,
    },
    assign,
    events::GlobalAccountDataEventType,
    DeviceId, OwnedUserId, TransactionId, UserId,
};
use tracing::{debug, instrument, trace, warn};

use crate::{
    attachment::{AttachmentInfo, Thumbnail},
    encryption::{
        identities::{Device, UserDevices},
        verification::{SasVerification, Verification, VerificationRequest},
    },
    error::{HttpResult, RoomKeyImportError},
    room, Client, Error, Result,
};

impl Client {
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn olm_machine(&self) -> Option<&matrix_sdk_base::crypto::OlmMachine> {
        self.base_client().olm_machine()
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
        response: impl Into<matrix_sdk_base::crypto::IncomingResponse<'_>>,
    ) -> Result<(), matrix_sdk_base::Error> {
        Ok(self
            .olm_machine()
            .expect(
                "We should have an olm machine once we try to mark E2EE related requests as sent",
            )
            .mark_request_as_sent(request_id, response)
            .await?)
    }

    /// Query the server for users device keys.
    ///
    /// # Panics
    ///
    /// Panics if no key query needs to be done.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self))]
    pub(crate) async fn keys_query(
        &self,
        request_id: &TransactionId,
        device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    ) -> Result<get_keys::v3::Response> {
        let request = assign!(get_keys::v3::Request::new(), { device_keys });

        let response = self.send(request, None).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    /// Encrypt and upload the file to be read from `reader` and construct an
    /// attachment message with `body`, `content_type`, `info` and `thumbnail`.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn prepare_encrypted_attachment_message(
        &self,
        body: &str,
        content_type: &mime::Mime,
        data: &[u8],
        info: Option<AttachmentInfo>,
        thumbnail: Option<Thumbnail<'_>>,
    ) -> Result<ruma::events::room::message::MessageType> {
        let (thumbnail_source, thumbnail_info) = if let Some(thumbnail) = thumbnail {
            let mut cursor = Cursor::new(thumbnail.data);
            let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(&mut cursor);

            let mut buf = Vec::new();
            encryptor.read_to_end(&mut buf)?;

            let response = self.upload(thumbnail.content_type, &buf).await?;

            let file: ruma::events::room::EncryptedFile = {
                let keys = encryptor.finish();
                ruma::events::room::EncryptedFileInit {
                    url: response.content_uri,
                    key: keys.key,
                    iv: keys.iv,
                    hashes: keys.hashes,
                    v: keys.version,
                }
                .into()
            };

            use ruma::events::room::ThumbnailInfo;

            #[rustfmt::skip]
            let thumbnail_info =
                assign!(thumbnail.info.map(ThumbnailInfo::from).unwrap_or_default(), {
                    mimetype: Some(thumbnail.content_type.as_ref().to_owned())
                });

            (Some(MediaSource::Encrypted(Box::new(file))), Some(Box::new(thumbnail_info)))
        } else {
            (None, None)
        };

        let mut cursor = Cursor::new(data);
        let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(&mut cursor);
        let mut buf = Vec::new();
        encryptor.read_to_end(&mut buf)?;

        let response = self.upload(content_type, &buf).await?;

        let file: ruma::events::room::EncryptedFile = {
            let keys = encryptor.finish();
            ruma::events::room::EncryptedFileInit {
                url: response.content_uri,
                key: keys.key,
                iv: keys.iv,
                hashes: keys.hashes,
                v: keys.version,
            }
            .into()
        };

        use std::io::Cursor;

        use ruma::events::room::{self, message, MediaSource};
        Ok(match content_type.type_() {
            mime::IMAGE => {
                let info = assign!(info.map(room::ImageInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content =
                    assign!(message::ImageMessageEventContent::encrypted(body.to_owned(), file), {
                        info: Some(Box::new(info))
                    });
                message::MessageType::Image(content)
            }
            mime::AUDIO => {
                let info = assign!(info.map(message::AudioInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                });
                let content =
                    assign!(message::AudioMessageEventContent::encrypted(body.to_owned(), file), {
                        info: Some(Box::new(info))
                    });
                message::MessageType::Audio(content)
            }
            mime::VIDEO => {
                let info = assign!(info.map(message::VideoInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content =
                    assign!(message::VideoMessageEventContent::encrypted(body.to_owned(), file), {
                        info: Some(Box::new(info))
                    });
                message::MessageType::Video(content)
            }
            _ => {
                let info = assign!(info.map(message::FileInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content =
                    assign!(message::FileMessageEventContent::encrypted(body.to_owned(), file), {
                        info: Some(Box::new(info))
                    });
                message::MessageType::File(content)
            }
        })
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn create_dm_room(
        &self,
        user_id: OwnedUserId,
    ) -> Result<Option<room::Joined>> {
        use ruma::{api::client::room::create_room::v3::RoomPreset, events::direct::DirectEvent};

        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        // First we create the DM room, where we invite the user and tell the
        // invitee that the room should be a DM.
        let invite = &[user_id.clone()];

        let request = assign!(ruma::api::client::room::create_room::v3::Request::new(), {
            invite,
            is_direct: true,
            preset: Some(RoomPreset::TrustedPrivateChat),
        });

        let response = self.send(request, None).await?;

        // Now we need to mark the room as a DM for ourselves, we fetch the
        // existing `m.direct` event and append the room to the list of DMs we
        // have with this user.
        let mut content = self
            .store()
            .get_account_data_event(GlobalAccountDataEventType::Direct)
            .await?
            .map(|e| e.deserialize_as::<DirectEvent>())
            .transpose()?
            .map(|e| e.content)
            .unwrap_or_else(|| ruma::events::direct::DirectEventContent(BTreeMap::new()));

        content.entry(user_id.to_owned()).or_default().push(response.room_id.to_owned());

        // TODO We should probably save the fact that we need to send this out
        // because otherwise we might end up in a state where we have a DM that
        // isn't marked as one.
        self.account().set_account_data(content).await?;

        // If the room is already in our store, fetch it, otherwise wait for a
        // sync to be done which should put the room into our store.
        if let Some(room) = self.get_joined_room(&response.room_id) {
            Ok(Some(room))
        } else {
            self.inner.sync_beat.listen().wait_timeout(SYNC_WAIT_TIME);
            Ok(self.get_joined_room(&response.room_id))
        }
    }

    /// Claim one-time keys creating new Olm sessions.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of user/device pairs that we should claim keys for.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    pub(crate) async fn claim_one_time_keys(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let _lock = self.inner.key_claim_lock.lock().await;

        if let Some((request_id, request)) = self
            .olm_machine()
            .ok_or(Error::AuthenticationRequired)?
            .get_missing_sessions(users)
            .await?
        {
            let response = self.send(request, None).await?;
            self.mark_request_as_sent(&request_id, &response).await?;
        }

        Ok(())
    }

    /// Upload the E2E encryption keys.
    ///
    /// This uploads the long lived device keys as well as the required amount
    /// of one-time keys.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, request))]
    pub(crate) async fn keys_upload(
        &self,
        request_id: &TransactionId,
        request: &upload_keys::v3::Request,
    ) -> Result<upload_keys::v3::Response> {
        debug!(
            device_keys = request.device_keys.is_some(),
            one_time_key_count = request.one_time_keys.len(),
            "Uploading public encryption keys",
        );

        let response = self.send(request.clone(), None).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn room_send_helper(
        &self,
        request: &RoomMessageRequest,
    ) -> Result<send_message_event::v3::Response> {
        let content = request.content.clone();
        let txn_id = &request.txn_id;
        let room_id = &request.room_id;

        self.get_joined_room(room_id)
            .expect("Can't send a message to a room that isn't known to the store")
            .send(content, Some(txn_id))
            .await
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn send_to_device(
        &self,
        request: &ToDeviceRequest,
    ) -> HttpResult<ToDeviceResponse> {
        let event_type = request.event_type.to_string();
        let request =
            RumaToDeviceRequest::new_raw(&event_type, &request.txn_id, request.messages.clone());

        self.send(request, None).await
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn send_verification_request(
        &self,
        request: matrix_sdk_base::crypto::OutgoingVerificationRequest,
    ) -> Result<()> {
        match request {
            matrix_sdk_base::crypto::OutgoingVerificationRequest::ToDevice(t) => {
                self.send_to_device(&t).await?;
            }
            matrix_sdk_base::crypto::OutgoingVerificationRequest::InRoom(r) => {
                self.room_send_helper(&r).await?;
            }
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    fn get_dm_room(&self, user_id: &UserId) -> Option<room::Joined> {
        let rooms = self.joined_rooms();

        // Find the room we share with the `user_id` and only with `user_id`
        let room = rooms.into_iter().find(|r| {
            let targets = r.direct_targets();
            targets.len() == 1 && targets.contains(user_id)
        });

        trace!(?room, "Found room");
        room
    }

    async fn send_outgoing_request(&self, r: OutgoingRequest) -> Result<()> {
        use matrix_sdk_base::crypto::OutgoingRequests;

        match r.request() {
            OutgoingRequests::KeysQuery(request) => {
                self.keys_query(r.request_id(), request.device_keys.clone()).await?;
            }
            OutgoingRequests::KeysUpload(request) => {
                self.keys_upload(r.request_id(), request).await?;
            }
            OutgoingRequests::ToDeviceRequest(request) => {
                let response = self.send_to_device(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::SignatureUpload(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::RoomMessage(request) => {
                let response = self.room_send_helper(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::KeysClaim(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::KeysBackup(request) => {
                let response = self.send_backup_request(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
        }

        Ok(())
    }

    async fn send_backup_request(
        &self,
        request: &matrix_sdk_base::crypto::KeysBackupRequest,
    ) -> Result<KeysBackupResponse> {
        let request = ruma::api::client::backup::add_backup_keys::v3::Request::new(
            &request.version,
            request.rooms.to_owned(),
        );

        Ok(self.send(request, None).await?)
    }

    pub(crate) async fn send_outgoing_requests(&self) -> Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 20;

        // This is needed because sometimes we need to automatically
        // claim some one-time keys to unwedge an existing Olm session.
        if let Err(e) = self.claim_one_time_keys(iter::empty()).await {
            warn!("Error while claiming one-time keys {:?}", e);
        }

        let outgoing_requests = stream::iter(
            self.olm_machine().ok_or(Error::AuthenticationRequired)?.outgoing_requests().await?,
        )
        .map(|r| self.send_outgoing_request(r));

        let requests = outgoing_requests.buffer_unordered(MAX_CONCURRENT_REQUESTS);

        requests
            .for_each(|r| async move {
                match r {
                    Ok(_) => (),
                    Err(e) => warn!(error = ?e, "Error when sending out an outgoing E2EE request"),
                }
            })
            .await;

        Ok(())
    }
}

/// A high-level API to manage the client's encryption.
///
/// To get this, use [`Client::encryption()`].
#[cfg(feature = "e2e-encryption")]
#[derive(Debug, Clone)]
pub struct Encryption {
    /// The underlying client.
    client: Client,
}

#[cfg(feature = "e2e-encryption")]
impl Encryption {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Get the public ed25519 key of our own device. This is usually what is
    /// called the fingerprint of the device.
    pub async fn ed25519_key(&self) -> Option<String> {
        self.client.olm_machine().map(|o| o.identity_keys().ed25519.to_base64())
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    pub async fn cross_signing_status(&self) -> Option<CrossSigningStatus> {
        if let Some(machine) = self.client.olm_machine() {
            Some(machine.cross_signing_status().await)
        } else {
            None
        }
    }

    /// Get all the tracked users we know about
    ///
    /// Tracked users are users for which we keep the device list of E2EE
    /// capable devices up to date.
    pub async fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.client.olm_machine().map(|o| o.tracked_users()).unwrap_or_default()
    }

    /// Get a verification object with the given flow id.
    pub async fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        let olm = self.client.olm_machine()?;
        #[allow(clippy::bind_instead_of_map)]
        olm.get_verification(user_id, flow_id).and_then(|v| match v {
            matrix_sdk_base::crypto::Verification::SasV1(s) => {
                Some(SasVerification { inner: s, client: self.client.clone() }.into())
            }
            #[cfg(feature = "qrcode")]
            matrix_sdk_base::crypto::Verification::QrV1(qr) => {
                Some(verification::QrVerification { inner: qr, client: self.client.clone() }.into())
            }
            _ => None,
        })
    }

    /// Get a `VerificationRequest` object for the given user with the given
    /// flow id.
    pub async fn get_verification_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        let olm = self.client.olm_machine()?;

        olm.get_verification_request(user_id, flow_id)
            .map(|r| VerificationRequest { inner: r, client: self.client.clone() })
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::{device_id, user_id}};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let Some(device) =
    ///     client.encryption().get_device(alice, device_id!("DEVICEID")).await?
    /// {
    ///     println!("{:?}", device.is_verified());
    ///
    ///     if !device.is_verified() {
    ///         let verification = device.request_verification().await?;
    ///     }
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>, CryptoStoreError> {
        if let Some(machine) = self.client.olm_machine() {
            let device = machine.get_device(user_id, device_id, None).await?;
            Ok(device.map(|d| Device { inner: d, client: self.client.clone() }))
        } else {
            Ok(None)
        }
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let devices = client.encryption().get_user_devices(alice).await?;
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices, Error> {
        let devices = self
            .client
            .olm_machine()
            .ok_or(Error::AuthenticationRequired)?
            .get_user_devices(user_id, None)
            .await?;

        Ok(UserDevices { inner: devices, client: self.client.clone() })
    }

    /// Get a E2EE identity of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to.
    ///
    /// Returns a `UserIdentity` if one is found and the crypto store
    /// didn't throw an error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("{:?}", user.is_verified());
    ///
    ///     let verification = user.request_verification().await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<crate::encryption::identities::UserIdentity>, CryptoStoreError> {
        use crate::encryption::identities::UserIdentity;

        if let Some(olm) = self.client.olm_machine() {
            let identity = olm.get_identity(user_id, None).await?;

            Ok(identity.map(|i| match i {
                matrix_sdk_base::crypto::UserIdentities::Own(i) => {
                    UserIdentity::new_own(self.client.clone(), i)
                }
                matrix_sdk_base::crypto::UserIdentities::Other(i) => {
                    UserIdentity::new(self.client.clone(), i, self.client.get_dm_room(user_id))
                }
            }))
        } else {
            Ok(None)
        }
    }

    /// Create and upload a new cross signing identity.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::collections::BTreeMap;
    /// # use matrix_sdk::{ruma::api::client::uiaa, Client};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let Err(e) = client.encryption().bootstrap_cross_signing(None).await {
    ///     if let Some(response) = e.uiaa_response() {
    ///         let mut password = uiaa::Password::new(
    ///             uiaa::UserIdentifier::UserIdOrLocalpart("example"),
    ///             "wordpass",
    ///         );
    ///         password.session = response.session.as_deref();
    ///
    ///         client
    ///             .encryption()
    ///             .bootstrap_cross_signing(Some(uiaa::AuthData::Password(password)))
    ///             .await
    ///             .expect("Couldn't bootstrap cross signing")
    ///     } else {
    ///         panic!("Error durign cross signing bootstrap {:#?}", e);
    ///     }
    /// }
    /// # anyhow::Ok(()) });
    pub async fn bootstrap_cross_signing(&self, auth_data: Option<AuthData<'_>>) -> Result<()> {
        let olm = self.client.olm_machine().ok_or(Error::AuthenticationRequired)?;

        let (request, signature_request) = olm.bootstrap_cross_signing(false).await?;

        let request = assign!(UploadSigningKeysRequest::new(), {
            auth: auth_data,
            master_key: request.master_key.map(|c| c.to_raw()),
            self_signing_key: request.self_signing_key.map(|c| c.to_raw()),
            user_signing_key: request.user_signing_key.map(|c| c.to_raw()),
        });

        self.client.send(request, None).await?;
        self.client.send(signature_request, None).await?;

        Ok(())
    }

    /// Export E2EE keys that match the given predicate encrypting them with the
    /// given passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will be saved.
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the
    ///   exported
    /// room keys.
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSessoin` will be included in the export,
    /// if the closure returns `false` it will not be included.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// This method will panic if it can't get enough randomness from the OS to
    /// encrypt the exported keys securely.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// // Export all room keys.
    /// client
    ///     .encryption()
    ///     .export_keys(path, "secret-passphrase", |_| true)
    ///     .await?;
    ///
    /// // Export only the room keys for a certain room.
    /// let path = PathBuf::from("/home/example/e2e-room-keys.txt");
    /// let room_id = room_id!("!test:localhost");
    ///
    /// client
    ///     .encryption()
    ///     .export_keys(path, "secret-passphrase", |s| s.room_id() == room_id)
    ///     .await?;
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn export_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
        predicate: impl FnMut(&matrix_sdk_base::crypto::olm::InboundGroupSession) -> bool,
    ) -> Result<()> {
        let olm = self.client.olm_machine().ok_or(Error::AuthenticationRequired)?;

        let keys = olm.export_keys(predicate).await?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let encrypt = move || -> Result<()> {
            let export: String =
                matrix_sdk_base::crypto::encrypt_key_export(&keys, &passphrase, 500_000)?;
            let mut file = std::fs::File::create(path)?;
            file.write_all(&export.into_bytes())?;
            Ok(())
        };

        let task = tokio::task::spawn_blocking(encrypt);
        task.await.expect("Task join error")
    }

    /// Import E2EE keys from the given file path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will can be found.
    ///
    /// * `passphrase` - The passphrase that should be used to decrypt the
    /// exported room keys.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// let result =
    ///     client.encryption().import_keys(path, "secret-passphrase").await?;
    ///
    /// println!(
    ///     "Imported {} room keys out of {}",
    ///     result.imported_count, result.total_count
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn import_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
    ) -> Result<RoomKeyImportResult, RoomKeyImportError> {
        let olm = self.client.olm_machine().ok_or(RoomKeyImportError::StoreClosed)?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let decrypt = move || {
            let file = std::fs::File::open(path)?;
            matrix_sdk_base::crypto::decrypt_key_export(file, &passphrase)
        };

        let task = tokio::task::spawn_blocking(decrypt);
        let import = task.await.expect("Task join error")?;

        Ok(olm.import_keys(import, false, |_, _| {}).await?)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_test::{async_test, test_json, EventBuilder, JoinedRoomBuilder, StateTestEvent};
    use ruma::{
        event_id,
        events::reaction::{ReactionEventContent, Relation},
    };
    use serde_json::json;
    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::test_utils::logged_in_client;

    #[async_test]
    async fn test_reaction_sending() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let event_id = event_id!("$2:example.org");
        let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;

        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/m%2Ereaction/.*".to_owned()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            })))
            .mount(&server)
            .await;

        let response = EventBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_state_event(StateTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels)
                    .add_state_event(StateTestEvent::Encryption),
            )
            .build_sync_response();

        client.inner.base_client.receive_sync_response(response).await.unwrap();

        let room = client.get_joined_room(room_id).expect("Room should exist");
        assert!(room.is_encrypted());

        let event_id = event_id!("$1:example.org");
        let reaction = ReactionEventContent::new(Relation::new(event_id.into(), "🐈".to_owned()));
        room.send(reaction, None).await.expect("Sending the reaction should not fail");

        room.send_raw(json!({}), "m.reaction", None)
            .await
            .expect("Sending the reaction should not fail");
    }
}
