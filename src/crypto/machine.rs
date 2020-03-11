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

use super::error::SignatureError;
use super::olm::Account;
use crate::api;

use api::r0::keys;

use cjson;
use olm_rs::utility::OlmUtility;
use serde_json::json;
use serde_json::Value;

use ruma_client_api::r0::keys::{
    AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey, SignedKey,
};
use ruma_events::Algorithm;
use ruma_identifiers::{DeviceId, UserId};

pub type OneTimeKeys = HashMap<AlgorithmAndDeviceId, OneTimeKey>;

#[derive(Debug)]
pub struct OlmMachine {
    /// The unique user id that owns this account.
    user_id: UserId,
    /// The unique device id of the device that holds this account.
    device_id: DeviceId,
    /// Our underlying Olm Account holding our identity keys.
    account: Account,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Option<u64>,
}

impl OlmMachine {
    const ALGORITHMS: &'static [&'static ruma_events::Algorithm] = &[
        &Algorithm::OlmV1Curve25519AesSha2,
        &Algorithm::MegolmV1AesSha2,
    ];

    /// Create a new account.
    pub fn new(user_id: &UserId, device_id: &str) -> Self {
        OlmMachine {
            user_id: user_id.clone(),
            device_id: device_id.to_owned(),
            account: Account::new(),
            uploaded_signed_key_count: None,
        }
    }

    /// Should account or one-time keys be uploaded to the server.
    pub fn should_upload_keys(&self) -> bool {
        if !self.account.shared() {
            return true;
        }

        // If we have a known key count, check that we have more than
        // max_one_time_Keys() / 2, otherwise tell the client to upload more.
        match self.uploaded_signed_key_count {
            Some(count) => {
                let max_keys = self.account.max_one_time_keys() as u64;
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
    pub async fn receive_keys_upload_response(&mut self, response: &keys::upload_keys::Response) {
        self.account.shared = true;

        let one_time_key_count = response
            .one_time_key_counts
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        let count: u64 = one_time_key_count.map_or(0, |c| (*c).into());
        self.uploaded_signed_key_count = Some(count);

        self.account.mark_keys_as_published();
        // TODO save the account here.
    }

    /// Generate new one-time keys.
    ///
    /// Returns the number of newly generated one-time keys. If no keys can be
    /// generated returns an empty error.
    fn generate_one_time_keys(&self) -> Result<u64, ()> {
        match self.uploaded_signed_key_count {
            Some(count) => {
                let max_keys = self.account.max_one_time_keys() as u64;
                let max_on_server = max_keys / 2;

                if count >= (max_on_server) {
                    return Err(());
                }

                let key_count = (max_on_server) - count;

                let key_count: usize = key_count
                    .try_into()
                    .unwrap_or_else(|_| self.account.max_one_time_keys());

                self.account.generate_one_time_keys(key_count);
                Ok(key_count as u64)
            }
            None => Err(()),
        }
    }

    /// Sign the device keys and return a JSON Value to upload them.
    fn device_keys(&self) -> DeviceKeys {
        let identity_keys = self.account.identity_keys();

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
            self.sign_json(&device_keys),
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
    fn signed_one_time_keys(&self) -> Result<OneTimeKeys, ()> {
        let _ = self.generate_one_time_keys()?;

        let one_time_keys = self.account.one_time_keys();
        let mut one_time_key_map = HashMap::new();

        for (key_id, key) in one_time_keys.curve25519().iter() {
            let key_json = json!({
                "key": key,
            });

            let signature = self.sign_json(&key_json);

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
    fn sign_json(&self, json: &Value) -> String {
        let canonical_json = cjson::to_string(json)
            .unwrap_or_else(|_| panic!(format!("Can't serialize {} to canonical JSON", json)));
        self.account.sign(&canonical_json)
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
    ) -> Result<(), SignatureError> {
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
    pub fn keys_for_upload(&self) -> Result<(Option<DeviceKeys>, Option<OneTimeKeys>), ()> {
        if !self.should_upload_keys() {
            return Err(());
        }

        let device_keys = if !self.account.shared() {
            Some(self.device_keys())
        } else {
            None
        };

        let one_time_keys: Option<OneTimeKeys> = self.signed_one_time_keys().ok();

        Ok((device_keys, one_time_keys))
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

    #[test]
    fn create_olm_machine() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID);
        assert!(machine.should_upload_keys());
    }

    #[async_std::test]
    async fn receive_keys_upload_response() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID);
        let mut response = keys_upload_response();

        response
            .one_time_key_counts
            .remove(&keys::KeyAlgorithm::SignedCurve25519)
            .unwrap();

        assert!(machine.should_upload_keys());
        machine.receive_keys_upload_response(&response).await;
        assert!(machine.should_upload_keys());

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(10).unwrap(),
        );
        machine.receive_keys_upload_response(&response).await;
        assert!(machine.should_upload_keys());

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(50).unwrap(),
        );
        machine.receive_keys_upload_response(&response).await;
        assert!(!machine.should_upload_keys());
    }

    #[async_std::test]
    async fn generate_one_time_keys() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID);

        let mut response = keys_upload_response();

        assert!(machine.should_upload_keys());
        assert!(machine.generate_one_time_keys().is_err());

        machine.receive_keys_upload_response(&response).await;
        assert!(machine.should_upload_keys());
        assert!(machine.generate_one_time_keys().is_ok());

        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            UInt::try_from(50).unwrap(),
        );
        machine.receive_keys_upload_response(&response).await;
        assert!(machine.generate_one_time_keys().is_err());
    }

    #[test]
    fn test_device_key_signing() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID);

        let mut device_keys = machine.device_keys();
        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            ed25519_key,
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_ok());
    }

    #[test]
    fn test_invalid_signature() {
        let machine = OlmMachine::new(&user_id(), DEVICE_ID);

        let mut device_keys = machine.device_keys();

        let ret = machine.verify_json(
            &machine.user_id,
            &machine.device_id,
            "fake_key",
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_err());
    }

    #[test]
    fn test_one_time_key_signing() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID);
        machine.uploaded_signed_key_count = Some(49);

        let mut one_time_keys = machine.signed_one_time_keys().unwrap();
        let identity_keys = machine.account.identity_keys();
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

    #[async_std::test]
    async fn test_keys_for_upload() {
        let mut machine = OlmMachine::new(&user_id(), DEVICE_ID);
        machine.uploaded_signed_key_count = Some(0);

        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let (device_keys, mut one_time_keys) = machine
            .keys_for_upload()
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

        machine.receive_keys_upload_response(&response).await;

        let ret = machine.keys_for_upload();
        assert!(ret.is_err());
    }
}
