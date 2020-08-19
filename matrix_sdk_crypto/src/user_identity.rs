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

use std::{
    collections::BTreeMap,
    convert::TryFrom,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use serde_json::to_value;

use matrix_sdk_common::{
    api::r0::keys::CrossSigningKey,
    identifiers::{DeviceKeyId, UserId},
};

use crate::{error::SignatureError, verify_json, ReadOnlyDevice};

#[derive(Debug, Clone)]
pub struct MasterPubkey(Arc<CrossSigningKey>);

#[derive(Debug, Clone)]
pub struct SelfSigningPubkey(Arc<CrossSigningKey>);

#[derive(Debug, Clone)]
pub struct UserSigningPubkey(Arc<CrossSigningKey>);

impl From<&CrossSigningKey> for MasterPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl From<&CrossSigningKey> for SelfSigningPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl From<&CrossSigningKey> for UserSigningPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl<'a> From<&'a SelfSigningPubkey> for CrossSigningSubKeys<'a> {
    fn from(key: &'a SelfSigningPubkey) -> Self {
        CrossSigningSubKeys::SelfSigning(key)
    }
}

impl<'a> From<&'a UserSigningPubkey> for CrossSigningSubKeys<'a> {
    fn from(key: &'a UserSigningPubkey) -> Self {
        CrossSigningSubKeys::UserSigning(key)
    }
}

enum CrossSigningSubKeys<'a> {
    SelfSigning(&'a SelfSigningPubkey),
    UserSigning(&'a UserSigningPubkey),
}

impl<'a> CrossSigningSubKeys<'a> {
    fn cross_signing_key(&self) -> &CrossSigningKey {
        match self {
            CrossSigningSubKeys::SelfSigning(key) => &key.0,
            CrossSigningSubKeys::UserSigning(key) => &key.0,
        }
    }
}

impl MasterPubkey {
    fn verify_subkey<'a>(
        &self,
        subkey: impl Into<CrossSigningSubKeys<'a>>,
    ) -> Result<(), SignatureError> {
        let (key_id, key) = self
            .0
            .keys
            .iter()
            .next()
            .ok_or(SignatureError::MissingSigningKey)?;

        // FIXME `KeyUsage is missing PartialEq.
        // if self.0.usage.contains(&KeyUsage::Master) {
        //     return Err(SignatureError::MissingSigningKey);
        // }

        let subkey: CrossSigningSubKeys = subkey.into();

        verify_json(
            &self.0.user_id,
            &DeviceKeyId::try_from(key_id.as_str())?,
            key,
            &mut to_value(subkey.cross_signing_key()).map_err(|_| SignatureError::NotAnObject)?,
        )
    }
}

impl UserSigningPubkey {
    fn verify_master_key(&self, master_key: &MasterPubkey) -> Result<(), SignatureError> {
        let (key_id, key) = self
            .0
            .keys
            .iter()
            .next()
            .ok_or(SignatureError::MissingSigningKey)?;

        // TODO check that the usage is OK.

        verify_json(
            &self.0.user_id,
            &DeviceKeyId::try_from(key_id.as_str())?,
            key,
            &mut to_value(&*master_key.0).map_err(|_| SignatureError::NotAnObject)?,
        )
    }
}

impl SelfSigningPubkey {
    fn verify_device(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        let (key_id, key) = self
            .0
            .keys
            .iter()
            .next()
            .ok_or(SignatureError::MissingSigningKey)?;

        // TODO check that the usage is OK.

        verify_json(
            &self.0.user_id,
            &DeviceKeyId::try_from(key_id.as_str())?,
            key,
            &mut device.as_signature_message(),
        )
    }
}

#[derive(Debug, Clone)]
pub enum UserIdentities {
    Own(OwnUserIdentity),
    Other(UserIdentity),
}

impl UserIdentities {
    pub fn user_id(&self) -> &UserId {
        match self {
            UserIdentities::Own(i) => i.user_id(),
            UserIdentities::Other(i) => i.user_id(),
        }
    }

    pub fn master_key(&self) -> &BTreeMap<String, String> {
        match self {
            UserIdentities::Own(i) => i.master_key(),
            UserIdentities::Other(i) => i.master_key(),
        }
    }

    pub fn own(&self) -> Option<&OwnUserIdentity> {
        match self {
            UserIdentities::Own(i) => Some(i),
            _ => None,
        }
    }

    pub fn other(&self) -> Option<&UserIdentity> {
        match self {
            UserIdentities::Other(i) => Some(i),
            _ => None,
        }
    }
}

impl PartialEq for UserIdentities {
    fn eq(&self, other: &UserIdentities) -> bool {
        self.user_id() == other.user_id()
    }
}

#[derive(Debug, Clone)]
pub struct UserIdentity {
    user_id: Arc<UserId>,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
}

impl UserIdentity {
    pub fn new(
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<Self, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        Ok(Self {
            user_id: Arc::new(master_key.0.user_id.clone()),
            master_key,
            self_signing_key,
        })
    }

    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn master_key(&self) -> &BTreeMap<String, String> {
        &self.master_key.0.keys
    }

    pub fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<(), SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        self.master_key = master_key;
        self.self_signing_key = self_signing_key;

        Ok(())
    }

    pub fn is_device_signed(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        self.self_signing_key.verify_device(device)
    }
}

#[derive(Debug, Clone)]
pub struct OwnUserIdentity {
    user_id: Arc<UserId>,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
    user_signing_key: UserSigningPubkey,
    verified: Arc<AtomicBool>,
}

impl OwnUserIdentity {
    pub fn new(
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        user_signing_key: UserSigningPubkey,
    ) -> Result<Self, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;
        master_key.verify_subkey(&user_signing_key)?;

        Ok(Self {
            user_id: Arc::new(master_key.0.user_id.clone()),
            master_key,
            self_signing_key,
            user_signing_key,
            verified: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        user_signing_key: UserSigningPubkey,
    ) -> Result<(), SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;
        master_key.verify_subkey(&user_signing_key)?;

        self.self_signing_key = self_signing_key;
        self.user_signing_key = user_signing_key;

        self.master_key = master_key;

        // FIXME reset the verification state if the master key changed.

        Ok(())
    }

    pub fn master_key(&self) -> &BTreeMap<String, String> {
        &self.master_key.0.keys
    }

    pub fn is_identity_signed(&self, identity: &UserIdentity) -> Result<(), SignatureError> {
        self.user_signing_key
            .verify_master_key(&identity.master_key)
    }

    pub fn is_device_signed(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        self.self_signing_key.verify_device(device)
    }

    pub fn mark_as_verified(&self) {
        self.verified.store(true, Ordering::SeqCst)
    }

    pub fn is_verified(&self) -> bool {
        self.verified.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod test {
    use serde_json::json;
    use std::convert::TryFrom;

    use matrix_sdk_common::{
        api::r0::keys::get_keys::Response as KeyQueryResponse, identifiers::user_id,
    };

    use crate::machine::test::response_from_file;

    use super::{OwnUserIdentity, UserIdentity};

    fn other_key_query() -> KeyQueryResponse {
        let data = response_from_file(&json!({
            "device_keys": {
                "@example2:localhost": {
                    "SKISMLNIMH": {
                        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                        "device_id": "SKISMLNIMH",
                        "keys": {
                            "curve25519:SKISMLNIMH": "qO9xFazIcW8dE0oqHGMojGgJwbBpMOhGnIfJy2pzvmI",
                            "ed25519:SKISMLNIMH": "y3wV3AoyIGREqrJJVH8DkQtlwHBUxoZ9ApP76kFgXQ8"
                        },
                        "signatures": {
                            "@example2:localhost": {
                                "ed25519:SKISMLNIMH": "YwbT35rbjKoYFZVU1tQP8MsL06+znVNhNzUMPt6jTEYRBFoC4GDq9hQEJBiFSq37r1jvLMteggVAWw37fs1yBA",
                                "ed25519:ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc": "PWuuTE/aTkp1EJQkPHhRx2BxbF+wjMIDFxDRp7JAerlMkDsNFUTfRRusl6vqROPU36cl+yY8oeJTZGFkU6+pBQ"
                            }
                        },
                        "user_id": "@example2:localhost",
                        "unsigned": {
                            "device_display_name": "Riot Desktop (Linux)"
                        }
                    }
                }
            },
            "failures": {},
            "master_keys": {
                "@example2:localhost": {
                    "user_id": "@example2:localhost",
                    "usage": ["master"],
                    "keys": {
                        "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do": "kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do"
                    },
                    "signatures": {
                        "@example2:localhost": {
                            "ed25519:SKISMLNIMH": "KdUZqzt8VScGNtufuQ8lOf25byYLWIhmUYpPENdmM8nsldexD7vj+Sxoo7PknnTX/BL9h2N7uBq0JuykjunCAw"
                        }
                    }
                }
            },
            "self_signing_keys": {
                "@example2:localhost": {
                    "user_id": "@example2:localhost",
                    "usage": ["self_signing"],
                    "keys": {
                        "ed25519:ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc": "ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc"
                    },
                    "signatures": {
                        "@example2:localhost": {
                            "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do": "W/O8BnmiUETPpH02mwYaBgvvgF/atXnusmpSTJZeUSH/vHg66xiZOhveQDG4cwaW8iMa+t9N4h1DWnRoHB4mCQ"
                        }
                    }
                }
            },
            "user_signing_keys": {}
        }));
        KeyQueryResponse::try_from(data).expect("Can't parse the keys upload response")
    }

    fn own_key_query() -> KeyQueryResponse {
        let data = response_from_file(&json!({
          "device_keys": {
          },
          "master_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"
                    },
                    "signatures": {
                        "@example:localhost": {
                            "ed25519:WSKKLTJZCL": "ZzJp1wtmRdykXAUEItEjNiFlBrxx8L6/Vaen9am8AuGwlxxJtOkuY4m+4MPLvDPOgavKHLsrRuNLAfCeakMlCQ"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@example:localhost"
                }
          },
          "self_signing_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210"
                    },
                    "signatures": {
                        "@example:localhost": {
                            "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "AC7oDUW4rUhtInwb4lAoBJ0wAuu4a5k+8e34B5+NKsDB8HXRwgVwUWN/MRWc/sJgtSbVlhzqS9THEmQQ1C51Bw"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@example:localhost"
                }
          },
          "user_signing_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo": "DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo"
                    },
                    "signatures": {
                        "@example:localhost": {
                            "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "C4L2sx9frGqj8w41KyynHGqwUbbwBYRZpYCB+6QWnvQFA5Oi/1PJj8w5anwzEsoO0TWmLYmf7FXuAGewanOWDg"
                        }
                    },
                    "usage": [
                        "user_signing"
                    ],
                    "user_id": "@example:localhost"
                }
          },
          "failures": {}
        }));

        KeyQueryResponse::try_from(data).expect("Can't parse the keys upload response")
    }

    #[test]
    fn own_identity_create() {
        let user_id = user_id!("@example:localhost");
        let response = own_key_query();

        let master_key = response.master_keys.get(&user_id).unwrap();
        let user_signing = response.user_signing_keys.get(&user_id).unwrap();
        let self_signing = response.self_signing_keys.get(&user_id).unwrap();

        OwnUserIdentity::new(master_key.into(), self_signing.into(), user_signing.into()).unwrap();
    }

    #[test]
    fn other_identity_create() {
        let user_id = user_id!("@example2:localhost");
        let response = other_key_query();

        let master_key = response.master_keys.get(&user_id).unwrap();
        let self_signing = response.self_signing_keys.get(&user_id).unwrap();

        UserIdentity::new(master_key.into(), self_signing.into()).unwrap();
    }
}
