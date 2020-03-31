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

use std::collections::HashMap;
use std::convert::TryInto;
#[cfg(feature = "sqlite-cryptostore")]
use std::path::Path;
use std::result::Result as StdResult;
use std::sync::Arc;

use super::error::{OlmError, Result, SignatureError, VerificationResult};
use super::memory_stores::SessionStore;
use super::olm::{Account, InboundGroupSession, Session};
use super::store::memorystore::MemoryStore;
#[cfg(feature = "sqlite-cryptostore")]
use super::store::sqlite::SqliteStore;
use super::CryptoStore;
use crate::api;

use api::r0::keys;

use cjson;
use olm_rs::{session::OlmMessage, utility::OlmUtility};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, trace, warn};

use ruma_client_api::r0::keys::{
    AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey, SignedKey,
};
use ruma_client_api::r0::sync::sync_events::IncomingResponse as SyncResponse;
use ruma_events::{
    collections::all::RoomEvent,
    room::encrypted::{EncryptedEvent, EncryptedEventContent},
    to_device::{
        AnyToDeviceEvent as ToDeviceEvent, ToDeviceEncrypted, ToDeviceForwardedRoomKey,
        ToDeviceRoomKey, ToDeviceRoomKeyRequest,
    },
    Algorithm, EventResult,
};
use ruma_identifiers::{DeviceId, UserId};

pub type OneTimeKeys = HashMap<AlgorithmAndDeviceId, OneTimeKey>;

#[derive(Debug)]
pub struct OlmMachine {
    /// The unique user id that owns this account.
    user_id: UserId,
    /// The unique device id of the device that holds this account.
    device_id: DeviceId,
    /// Our underlying Olm Account holding our identity keys.
    account: Arc<Mutex<Account>>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Option<u64>,
    /// Store for the encryption keys.
    /// Persists all the encrytpion keys so a client can resume the session
    /// without the need to create new keys.
    store: Box<dyn CryptoStore>,
}

impl OlmMachine {
    const ALGORITHMS: &'static [&'static ruma_events::Algorithm] = &[
        &Algorithm::OlmV1Curve25519AesSha2,
        &Algorithm::MegolmV1AesSha2,
    ];

    /// Create a new account.
    pub fn new(user_id: &UserId, device_id: &str) -> Result<Self> {
        Ok(OlmMachine {
            user_id: user_id.clone(),
            device_id: device_id.to_owned(),
            account: Arc::new(Mutex::new(Account::new())),
            uploaded_signed_key_count: None,
            store: Box::new(MemoryStore::new()),
        })
    }

    #[cfg(feature = "sqlite-cryptostore")]
    #[instrument(skip(path, passphrase))]
    pub async fn new_with_sqlite_store<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &str,
        path: P,
        passphrase: String,
    ) -> Result<Self> {
        let mut store =
            SqliteStore::open_with_passphrase(&user_id.to_string(), device_id, path, passphrase)
                .await?;

        let account = match store.load_account().await? {
            Some(a) => {
                debug!("Restored account");
                a
            }
            None => {
                debug!("Creating a new account");
                Account::new()
            }
        };

        Ok(OlmMachine {
            user_id: user_id.clone(),
            device_id: device_id.to_owned(),
            account: Arc::new(Mutex::new(account)),
            uploaded_signed_key_count: None,
            store: Box::new(store),
        })
    }

    /// Should account or one-time keys be uploaded to the server.
    pub async fn should_upload_keys(&self) -> bool {
        if !self.account.lock().await.shared() {
            return true;
        }

        // If we have a known key count, check that we have more than
        // max_one_time_Keys() / 2, otherwise tell the client to upload more.
        match self.uploaded_signed_key_count {
            Some(count) => {
                let max_keys = self.account.lock().await.max_one_time_keys() as u64;
                let key_count = (max_keys / 2) - count;
                key_count > 0
            }
            None => false,
        }
    }

    /// Receive a successful keys upload response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys upload response of the request that the client
    /// performed.
    #[instrument]
    pub async fn receive_keys_upload_response(
        &mut self,
        response: &keys::upload_keys::Response,
    ) -> Result<()> {
        let mut account = self.account.lock().await;
        if !account.shared {
            debug!("Marking account as shared");
        }
        account.shared = true;

        let one_time_key_count = response
            .one_time_key_counts
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        let count: u64 = one_time_key_count.map_or(0, |c| (*c).into());
        debug!(
            "Updated uploaded one-time key count {} -> {}, marking keys as published",
            self.uploaded_signed_key_count.as_ref().map_or(0, |c| *c),
            count
        );
        self.uploaded_signed_key_count = Some(count);

        account.mark_keys_as_published();
        drop(account);

        self.store.save_account(self.account.clone()).await?;

        Ok(())
    }

    /// Generate new one-time keys.
    ///
    /// Returns the number of newly generated one-time keys. If no keys can be
    /// generated returns an empty error.
    async fn generate_one_time_keys(&self) -> StdResult<u64, ()> {
        let account = self.account.lock().await;
        match self.uploaded_signed_key_count {
            Some(count) => {
                let max_keys = account.max_one_time_keys() as u64;
                let max_on_server = max_keys / 2;

                if count >= (max_on_server) {
                    return Err(());
                }

                let key_count = (max_on_server) - count;

                let key_count: usize = key_count
                    .try_into()
                    .unwrap_or_else(|_| account.max_one_time_keys());

                account.generate_one_time_keys(key_count);
                Ok(key_count as u64)
            }
            None => Err(()),
        }
    }

    /// Sign the device keys and return a JSON Value to upload them.
    async fn device_keys(&self) -> DeviceKeys {
        let identity_keys = self.account.lock().await.identity_keys();

        let mut keys = HashMap::new();

        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Curve25519, self.device_id.clone()),
            identity_keys.curve25519().to_owned(),
        );
        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, self.device_id.clone()),
            identity_keys.ed25519().to_owned(),
        );

        let device_keys = json!({
            "user_id": self.user_id,
            "device_id": self.device_id,
            "algorithms": OlmMachine::ALGORITHMS,
            "keys": keys,
        });

        let mut signatures = HashMap::new();

        let mut signature = HashMap::new();
        signature.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, self.device_id.clone()),
            self.sign_json(&device_keys).await,
        );
        signatures.insert(self.user_id.clone(), signature);

        DeviceKeys {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            algorithms: vec![
                Algorithm::OlmV1Curve25519AesSha2,
                Algorithm::MegolmV1AesSha2,
            ],
            keys,
            signatures,
            unsigned: None,
        }
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty error.
    async fn signed_one_time_keys(&self) -> StdResult<OneTimeKeys, ()> {
        let _ = self.generate_one_time_keys().await?;
        let one_time_keys = self.account.lock().await.one_time_keys();
        let mut one_time_key_map = HashMap::new();

        for (key_id, key) in one_time_keys.curve25519().iter() {
            let key_json = json!({
                "key": key,
            });

            let signature = self.sign_json(&key_json).await;

            let mut signature_map = HashMap::new();

            signature_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, self.device_id.clone()),
                signature,
            );

            let mut signatures = HashMap::new();
            signatures.insert(self.user_id.clone(), signature_map);

            let signed_key = SignedKey {
                key: key.to_owned(),
                signatures,
            };

            one_time_key_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::SignedCurve25519, key_id.to_owned()),
                OneTimeKey::SignedKey(signed_key),
            );
        }

        Ok(one_time_key_map)
    }

    /// Convert a JSON value to the canonical representation and sign the JSON
    /// string.
    ///
    /// # Arguments
    ///
    /// * `json` - The value that should be converted into a canonical JSON
    /// string.
    async fn sign_json(&self, json: &Value) -> String {
        let account = self.account.lock().await;
        let canonical_json = cjson::to_string(json)
            .unwrap_or_else(|_| panic!(format!("Can't serialize {} to canonical JSON", json)));
        account.sign(&canonical_json)
    }

    /// Verify a signed JSON object.
    ///
    /// The object must have a signatures key associated  with an object of the
    /// form `user_id: {key_id: signature}`.
    ///
    /// Returns Ok if the signature was successfully verified, otherwise an
    /// SignatureError.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user who signed the JSON object.
    ///
    /// * `device_id` - The device that signed the JSON object.
    ///
    /// * `user_key` - The public ed25519 key which was used to sign the JSON
    ///     object.
    ///
    /// * `json` - The JSON object that should be verified.
    fn verify_json(
        &self,
        user_id: &UserId,
        device_id: &str,
        user_key: &str,
        json: &mut Value,
    ) -> VerificationResult<()> {
        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let unsigned = json_object.remove("unsigned");
        let signatures = json_object.remove("signatures");

        let canonical_json = cjson::to_string(json_object)?;

        if let Some(u) = unsigned {
            json_object.insert("unsigned".to_string(), u);
        }

        // TODO this should be part of ruma-client-api.
        let key_id_string = format!("{}:{}", KeyAlgorithm::Ed25519, device_id);

        let signatures = signatures.ok_or(SignatureError::NoSignatureFound)?;
        let signature_object = signatures
            .as_object()
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature_object
            .get(&user_id.to_string())
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature
            .get(key_id_string)
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature.as_str().ok_or(SignatureError::NoSignatureFound)?;

        let utility = OlmUtility::new();

        let ret = if utility
            .ed25519_verify(&user_key, &canonical_json, signature)
            .is_ok()
        {
            Ok(())
        } else {
            Err(SignatureError::VerificationError)
        };

        json_object.insert("signatures".to_string(), signatures);

        ret
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub async fn keys_for_upload(
        &self,
    ) -> StdResult<(Option<DeviceKeys>, Option<OneTimeKeys>), ()> {
        if !self.should_upload_keys().await {
            return Err(());
        }

        let shared = self.account.lock().await.shared();

        let device_keys = if !shared {
            Some(self.device_keys().await)
        } else {
            None
        };

        let one_time_keys: Option<OneTimeKeys> = self.signed_one_time_keys().await.ok();

        Ok((device_keys, one_time_keys))
    }

    async fn try_decrypt_olm_event(
        &mut self,
        sender_key: &str,
        message: &OlmMessage,
    ) -> Result<Option<String>> {
        let s = self.store.get_sessions(sender_key).await?;

        let sessions = if let Some(s) = s {
            s
        } else {
            return Ok(None);
        };

        for session in &*sessions.lock().await {
            let mut matches = false;

            let mut session_lock = session.lock().await;

            if let OlmMessage::PreKey(m) = &message {
                matches = session_lock.matches(sender_key, m.clone())?;
                if !matches {
                    continue;
                }
            }

            let ret = session_lock.decrypt(message.clone());

            if let Ok(p) = ret {
                self.store.save_session(session.clone()).await?;
                return Ok(Some(p));
            } else {
                if matches {
                    return Err(OlmError::SessionWedged);
                }
            }
        }

        Ok(None)
    }

    async fn decrypt_olm_message(
        &mut self,
        sender: &str,
        sender_key: &str,
        message: OlmMessage,
    ) -> Result<EventResult<ToDeviceEvent>> {
        let plaintext = if let Some(p) = self.try_decrypt_olm_event(sender_key, &message).await? {
            p
        } else {
            let mut session = match &message {
                OlmMessage::Message(_) => return Err(OlmError::SessionWedged),
                OlmMessage::PreKey(m) => {
                    let account = self.account.lock().await;
                    account.create_inbound_session_from(sender_key, m.clone())?
                }
            };

            let plaintext = session.decrypt(message)?;
            self.store.add_and_save_session(session).await?;
            plaintext
        };

        trace!("Successfully decrypted a Olm message: {}", plaintext);
        Ok(serde_json::from_str::<EventResult<ToDeviceEvent>>(
            &plaintext,
        )?)
    }

    /// Decrypt a to-device event.
    ///
    /// Returns a decrypted `ToDeviceEvent` if the decryption was successful,
    /// an error indicating why decryption failed otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The to-device event that should be decrypted.
    #[instrument]
    async fn decrypt_to_device_event(
        &mut self,
        event: &ToDeviceEncrypted,
    ) -> Result<EventResult<ToDeviceEvent>> {
        info!("Decrypting to-device event");

        let content = if let EncryptedEventContent::OlmV1Curve25519AesSha2(c) = &event.content {
            c
        } else {
            warn!("Error, unsupported encryption algorithm");
            return Err(OlmError::UnsupportedAlgorithm);
        };

        let identity_keys = self.account.lock().await.identity_keys();
        let own_key = identity_keys.curve25519();
        let own_ciphertext = content.ciphertext.get(own_key);

        if let Some(ciphertext) = own_ciphertext {
            let message_type: u8 = ciphertext
                .message_type
                .try_into()
                .map_err(|_| OlmError::UnsupportedOlmType)?;
            let message =
                OlmMessage::from_type_and_ciphertext(message_type.into(), ciphertext.body.clone())
                    .map_err(|_| OlmError::UnsupportedOlmType)?;

            let decrypted_event = self
                .decrypt_olm_message(&event.sender.to_string(), &content.sender_key, message)
                .await?;
            debug!("Decrypted a to-device event {:?}", decrypted_event);
            self.handle_decrypted_to_device_event(&content.sender_key, &decrypted_event)
                .await?;

            Ok(decrypted_event)
        } else {
            warn!("Olm event doesn't contain a ciphertext for our key");
            Err(OlmError::MissingCiphertext)
        }
    }

    async fn add_room_key(&mut self, sender_key: &str, event: &ToDeviceRoomKey) -> Result<()> {
        match event.content.algorithm {
            Algorithm::MegolmV1AesSha2 => {
                // TODO check for all the valid fields.
                let signing_key = event
                    .keys
                    .get("ed25519")
                    .ok_or(OlmError::MissingSigningKey)?;

                let session = InboundGroupSession::new(
                    sender_key,
                    signing_key,
                    &event.content.room_id.to_string(),
                    &event.content.session_key,
                )?;
                self.store.save_inbound_group_session(session).await?;
                Ok(())
            }
            _ => {
                warn!(
                    "Received room key with unsupported key algorithm {}",
                    event.content.algorithm
                );
                Ok(())
            }
        }
    }

    fn add_forwarded_room_key(
        &self,
        sender_key: &str,
        event: &ToDeviceForwardedRoomKey,
    ) -> Result<()> {
        Ok(())
        // TODO
    }

    async fn handle_decrypted_to_device_event(
        &mut self,
        sender_key: &str,
        event: &EventResult<ToDeviceEvent>,
    ) -> Result<()> {
        let event = if let EventResult::Ok(e) = event {
            e
        } else {
            warn!("Decrypted to-device event failed to be parsed correctly");
            return Ok(());
        };

        match event {
            ToDeviceEvent::RoomKey(e) => self.add_room_key(sender_key, e).await,
            ToDeviceEvent::ForwardedRoomKey(e) => self.add_forwarded_room_key(sender_key, e),
            _ => {
                warn!("Received a unexpected encrypted to-device event");
                Ok(())
            }
        }
    }

    fn handle_room_key_request(&self, _: &ToDeviceRoomKeyRequest) {
        // TODO handle room key requests here.
    }

    fn handle_verification_event(&self, _: &ToDeviceEvent) {
        // TODO handle to-device verification events here.
    }

    #[instrument(skip(response))]
    /// Handle a sync response and update the internal state of the Olm machine.
    ///
    /// This will decrypt to-device events but will not touch room messages.
    ///
    /// # Arguments
    ///
    /// * `response` - The sync latest sync response.
    pub async fn receive_sync_response(&mut self, response: &mut SyncResponse) {
        let one_time_key_count = response
            .device_one_time_keys_count
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        let count: u64 = one_time_key_count.map_or(0, |c| (*c).into());
        self.uploaded_signed_key_count = Some(count);

        for event_result in &mut response.to_device.events {
            let event = if let EventResult::Ok(e) = &event_result {
                e
            } else {
                // Skip invalid events.
                warn!("Received an invalid to-device event {:?}", event_result);
                continue;
            };

            info!("Received a to-device event {:?}", event);

            match event {
                ToDeviceEvent::RoomEncrypted(e) => {
                    let decrypted_event = match self.decrypt_to_device_event(e).await {
                        Ok(e) => e,
                        Err(err) => {
                            warn!(
                                "Failed to decrypt to-device event from {} {}",
                                e.sender, err
                            );
                            // TODO if the session is wedged mark it for
                            // unwedging.
                            continue;
                        }
                    };

                    // TODO make sure private keys are cleared from the event
                    // before we replace the result.
                    *event_result = decrypted_event;
                }
                ToDeviceEvent::RoomKeyRequest(e) => self.handle_room_key_request(e),
                ToDeviceEvent::KeyVerificationAccept(..)
                | ToDeviceEvent::KeyVerificationCancel(..)
                | ToDeviceEvent::KeyVerificationKey(..)
                | ToDeviceEvent::KeyVerificationMac(..)
                | ToDeviceEvent::KeyVerificationRequest(..)
                | ToDeviceEvent::KeyVerificationStart(..) => self.handle_verification_event(event),
                _ => continue,
            }
        }
    }

    pub async fn decrypt_room_event(
        &mut self,
        event: &EncryptedEvent,
    ) -> Result<EventResult<RoomEvent>> {
        let content = match &event.content {
            EncryptedEventContent::MegolmV1AesSha2(c) => c,
            _ => return Err(OlmError::UnsupportedAlgorithm),
        };

        let room_id = event.room_id.as_ref().unwrap();

        let session = self
            .store
            .get_inbound_group_session(
                &room_id.to_string(),
                &content.sender_key,
                &content.session_id,
            )
            .await?;
        // TODO check if the olm session is wedged and re-request the key.
        let session = session.ok_or(OlmError::MissingSession)?;

        let (plaintext, _) = session.lock().await.decrypt(content.ciphertext.clone())?;
        // TODO check the message index.
        // TODO check if this is from a verified device.

        let mut decrypted_value = serde_json::from_str::<Value>(&plaintext)?;
        let decrypted_object = decrypted_value
            .as_object_mut()
            .ok_or(OlmError::NotAnObject)?;

        let server_ts: u64 = event.origin_server_ts.into();

        decrypted_object.insert("sender".to_owned(), event.sender.to_string().into());
        decrypted_object.insert("event_id".to_owned(), event.event_id.to_string().into());
        decrypted_object.insert("origin_server_ts".to_owned(), server_ts.into());

        if let Some(unsigned) = &event.unsigned {
            decrypted_object.insert("unsigned".to_owned(), unsigned.clone());
        }

        let decrypted_event = serde_json::from_value::<EventResult<RoomEvent>>(decrypted_value)?;
        trace!("Successfully decrypted megolm event {:?}", decrypted_event);
        // TODO set the encryption info on the event (is it verified, was it
        // decrypted, sender key...)

        Ok(decrypted_event)
    }
}

#[cfg(test)]
mod test {
    static USER_ID: &str = "@test:example.org";
    const DEVICE_ID: &str = "DEVICEID";

    use js_int::UInt;
    use std::convert::TryFrom;
    use std::fs::File;
    use std::io::prelude::*;

    use ruma_identifiers::UserId;
    use serde_json::json;

    use crate::api::r0::keys;
    use crate::crypto::machine::OlmMachine;
    use http::Response;

    fn user_id() -> UserId {
        UserId::try_from(USER_ID).unwrap()
    }

    fn response_from_file(path: &str) -> Response<Vec<u8>> {
        let mut file = File::open(path)
            .unwrap_or_else(|_| panic!(format!("No such data file found {}", path)));
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .unwrap_or_else(|_| panic!(format!("Can't read data file {}", path)));

        Response::builder().status(200).body(contents).unwrap()
    }

    fn keys_upload_response() -> keys::upload_keys::Response {
        let data = response_from_file("tests/data/keys_upload.json");
        keys::upload_keys::Response::try_from(data).expect("Can't parse the keys upload response")
    }

    #[tokio::test]
    async fn create_olm_machine() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();
        assert!(machine.should_upload_keys().await);
    }

    #[tokio::test]
    async fn receive_keys_upload_response() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();
        let mut response = keys_upload_response();

        response
            .one_time_key_counts
            .remove(&keys::KeyAlgorithm::SignedCurve25519)
            .unwrap();

        assert!(machine.should_upload_keys().await);
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(10).unwrap(),
        );
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(50).unwrap(),
        );
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(!machine.should_upload_keys().await);
    }

    #[tokio::test]
    async fn generate_one_time_keys() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();

        let mut response = keys_upload_response();

        assert!(machine.should_upload_keys().await);
        assert!(machine.generate_one_time_keys().await.is_err());

        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);
        assert!(machine.generate_one_time_keys().await.is_ok());

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(50).unwrap(),
        );
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.generate_one_time_keys().await.is_err());
    }

    #[tokio::test]
    async fn test_device_key_signing() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();

        let mut device_keys = machine.device_keys().await;
        let identity_keys = machine.account.lock().await.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            ed25519_key,
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn test_invalid_signature() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();

        let mut device_keys = machine.device_keys().await;

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            "fake_key",
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_err());
    }

    #[tokio::test]
    async fn test_one_time_key_signing() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();
        machine.uploaded_signed_key_count = Some(49);

        let mut one_time_keys = machine.signed_one_time_keys().await.unwrap();
        let identity_keys = machine.account.lock().await.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let mut one_time_key = one_time_keys.values_mut().nth(0).unwrap();

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            ed25519_key,
            &mut json!(&mut one_time_key),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn test_keys_for_upload() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID).unwrap();
        machine.uploaded_signed_key_count = Some(0);

        let identity_keys = machine.account.lock().await.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let (device_keys, mut one_time_keys) = machine
            .keys_for_upload()
            .await
            .expect("Can't prepare initial key upload");

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            ed25519_key,
            &mut json!(&mut one_time_keys.as_mut().unwrap().values_mut().nth(0)),
        );
        assert!(ret.is_ok());

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            ed25519_key,
            &mut json!(&mut device_keys.unwrap()),
        );
        assert!(ret.is_ok());

        let mut response = keys_upload_response();
        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::new_wrapping(one_time_keys.unwrap().len() as u64),
        );

        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();

        let ret = machine.keys_for_upload().await;
        assert!(ret.is_err());
    }
}
