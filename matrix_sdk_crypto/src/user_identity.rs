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
    convert::TryFrom,
    sync::{atomic::AtomicBool, Arc},
};

use serde_json::to_value;

use matrix_sdk_common::{
    api::r0::keys::CrossSigningKey,
    identifiers::{DeviceKeyId, UserId},
};

use crate::{error::SignatureError, verify_json};

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

pub struct UserIdentity {
    user_id: Arc<UserId>,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
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

impl UserIdentity {
    pub fn new(
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<Self, SignatureError> {
        master_key.verify_subkey(&self_signing_key.clone())?;

        Ok(Self {
            user_id: Arc::new(master_key.0.user_id.clone()),
            master_key,
            self_signing_key,
        })
    }
}

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
        master_key.verify_subkey(&self_signing_key.clone())?;
        master_key.verify_subkey(&user_signing_key.clone())?;

        Ok(Self {
            user_id: Arc::new(master_key.0.user_id.clone()),
            master_key,
            self_signing_key,
            user_signing_key,
            verified: Arc::new(AtomicBool::new(false)),
        })
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

    use super::OwnUserIdentity;

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
}
