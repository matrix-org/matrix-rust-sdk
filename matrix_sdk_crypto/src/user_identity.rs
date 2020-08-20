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

/// Wrapper for a cross signing key marking it as the master key.
#[derive(Debug, Clone)]
pub struct MasterPubkey(Arc<CrossSigningKey>);

/// Wrapper for a cross signing key marking it as a self signing key.
#[derive(Debug, Clone)]
pub struct SelfSigningPubkey(Arc<CrossSigningKey>);

/// Wrapper for a cross signing key marking it as a user signing key.
#[derive(Debug, Clone)]
pub struct UserSigningPubkey(Arc<CrossSigningKey>);

impl PartialEq for MasterPubkey {
    fn eq(&self, other: &MasterPubkey) -> bool {
        self.0.user_id == other.0.user_id && self.0.keys == other.0.keys
        // TODO check the usage once `KeyUsage` gets PartialEq.
    }
}

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

/// Enum over the cross signing sub-keys.
enum CrossSigningSubKeys<'a> {
    /// The self signing subkey.
    SelfSigning(&'a SelfSigningPubkey),
    /// The user signing subkey.
    UserSigning(&'a UserSigningPubkey),
}

impl<'a> CrossSigningSubKeys<'a> {
    /// Get the id of the user that owns this cross signing subkey.
    fn user_id(&self) -> &UserId {
        match self {
            CrossSigningSubKeys::SelfSigning(key) => &key.0.user_id,
            CrossSigningSubKeys::UserSigning(key) => &key.0.user_id,
        }
    }

    /// Get the `CrossSigningKey` from an sub-keys enum
    fn cross_signing_key(&self) -> &CrossSigningKey {
        match self {
            CrossSigningSubKeys::SelfSigning(key) => &key.0,
            CrossSigningSubKeys::UserSigning(key) => &key.0,
        }
    }
}

impl MasterPubkey {
    /// Get the master key with the given key id.
    ///
    /// # Arguments
    ///
    /// * `key_id` - The id of the key that should be fetched.
    pub fn get_key(&self, key_id: &DeviceKeyId) -> Option<&str> {
        self.0.keys.get(key_id.as_str()).map(|k| k.as_str())
    }

    /// Check if the given cross signing sub-key is signed by the master key.
    ///
    /// # Arguments
    ///
    /// * `subkey` - The subkey that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
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
    /// Check if the given master key is signed by this user signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key that should be checked for a valid
    /// signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
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
    /// Check if the given device is signed by this self signing key.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
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

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone)]
pub enum UserIdentities {
    /// Our own user identity.
    Own(OwnUserIdentity),
    /// Identities of other users.
    Other(UserIdentity),
}

impl UserIdentities {
    /// The unique user id of this identity.
    pub fn user_id(&self) -> &UserId {
        match self {
            UserIdentities::Own(i) => i.user_id(),
            UserIdentities::Other(i) => i.user_id(),
        }
    }

    /// Get the master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        match self {
            UserIdentities::Own(i) => i.master_key(),
            UserIdentities::Other(i) => i.master_key(),
        }
    }

    /// Destructure the enum into an `OwnUserIdentity` if it's of the correct
    /// type.
    pub fn own(&self) -> Option<&OwnUserIdentity> {
        match self {
            UserIdentities::Own(i) => Some(i),
            _ => None,
        }
    }
}

impl PartialEq for UserIdentities {
    fn eq(&self, other: &UserIdentities) -> bool {
        self.user_id() == other.user_id()
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
#[derive(Debug, Clone)]
pub struct UserIdentity {
    user_id: Arc<UserId>,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
}

impl UserIdentity {
    /// Create a new user identity with the given master and self signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key of the user identity.
    ///
    /// * `self signing key` - The self signing key of user identity.
    ///
    /// Returns a `SignatureError` if the self signing key fails to be correctly
    /// verified by the given master key.
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

    /// Get the user id of this identity.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the public master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        &self.master_key
    }

    /// Update the identity with a new master key and self signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The new master key of the user identity.
    ///
    /// * `self_signing_key` - The new self signing key of user identity.
    ///
    /// Returns a `SignatureError` if we failed to update the identity.
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

    /// Check if the given device has been signed by this identity.
    ///
    /// The user_id of the user identity and the user_id of the device need to
    /// match for the signature check to succeed as we don't trust users to sign
    /// devices of other users.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
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
    /// Create a new own user identity with the given master, self signing, and
    /// user signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key of the user identity.
    ///
    /// * `self_signing_key` - The self signing key of user identity.
    ///
    /// * `user_signing_key` - The user signing key of user identity.
    ///
    /// Returns a `SignatureError` if the self signing key fails to be correctly
    /// verified by the given master key.
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

    /// Get the user id of this identity.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the public master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        &self.master_key
    }

    /// Check if the given identity has been signed by this identity.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity of another user that we want to check if
    /// it's has been signed.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub fn is_identity_signed(&self, identity: &UserIdentity) -> Result<(), SignatureError> {
        self.user_signing_key
            .verify_master_key(&identity.master_key)
    }

    /// Check if the given device has been signed by this identity.
    ///
    /// Only devices of our own user should be checked with this method, if a
    /// device of a different user is given the signature check will always fail
    /// even if a valid signature exists.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub fn is_device_signed(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        self.self_signing_key.verify_device(device)
    }

    /// Mark our identity as verified.
    pub fn mark_as_verified(&self) {
        self.verified.store(true, Ordering::SeqCst)
    }

    /// Check if our identity is verified.
    pub fn is_verified(&self) -> bool {
        self.verified.load(Ordering::SeqCst)
    }

    /// Update the identity with a new master key and self signing key.
    ///
    /// Note: This will reset the verification state if the master keys differ.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The new master key of the user identity.
    ///
    /// * `self_signing_key` - The new self signing key of user identity.
    ///
    /// * `user_signing_key` - The new user signing key of user identity.
    ///
    /// Returns a `SignatureError` if we failed to update the identity.
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

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use serde_json::json;
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_common::{
        api::r0::keys::get_keys::Response as KeyQueryResponse, identifiers::user_id,
    };

    use crate::{
        device::{Device, ReadOnlyDevice},
        machine::test::response_from_file,
        olm::Account,
        store::memorystore::MemoryStore,
        verification::VerificationMachine,
    };

    use super::{OwnUserIdentity, UserIdentities, UserIdentity};

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
            "@example:localhost": {
              "WSKKLTJZCL": {
                "algorithms": [
                  "m.olm.v1.curve25519-aes-sha2",
                  "m.megolm.v1.aes-sha2"
                ],
                "device_id": "WSKKLTJZCL",
                "keys": {
                  "curve25519:WSKKLTJZCL": "wnip2tbJBJxrFayC88NNJpm61TeSNgYcqBH4T9yEDhU",
                  "ed25519:WSKKLTJZCL": "lQ+eshkhgKoo+qp9Qgnj3OX5PBoWMU5M9zbuEevwYqE"
                },
                "signatures": {
                  "@example:localhost": {
                    "ed25519:WSKKLTJZCL": "SKpIUnq7QK0xleav0PrIQyKjVm+TgZr7Yi8cKjLeZDtkgyToE2d4/e3Aj79dqOlLB92jFVE4d1cM/Ry04wFwCA",
                    "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "9UGu1iC5YhFCdELGfB29YaV+QE0t/X5UDSsPf4QcdZyXIwyp9zBbHX2lh9vWudNQ+akZpaq7ZRaaM+4TCnw/Ag"
                  }
                },
                "user_id": "@example:localhost",
                "unsigned": {
                  "device_display_name": "Cross signing capable"
                }
              },
              "LVWOVGOXME": {
                "algorithms": [
                  "m.olm.v1.curve25519-aes-sha2",
                  "m.megolm.v1.aes-sha2"
                ],
                "device_id": "LVWOVGOXME",
                "keys": {
                  "curve25519:LVWOVGOXME": "KMfWKUhnDW1D11hNzATs/Ax1FQRsJxKCWzq0NyGtIiI",
                  "ed25519:LVWOVGOXME": "k+NC3L7CBD6fBClcHBrKLOkqCyGNSKhWXiH5Q2STRnA"
                },
                "signatures": {
                  "@example:localhost": {
                    "ed25519:LVWOVGOXME": "39Ir5Bttpc5+bQwzLj7rkjm5E5/cp/JTbMJ/t0enj6J5w9MXVBFOUqqM2hpaRaRwILMMpwYbJ8IOGjl0Y/MGAw"
                  }
                },
                "user_id": "@example:localhost",
                "unsigned": {
                  "device_display_name": "Non-cross signing"
                }
              }
            }
          },
          "failures": {},
          "master_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "master"
              ],
              "keys": {
                "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:WSKKLTJZCL": "ZzJp1wtmRdykXAUEItEjNiFlBrxx8L6/Vaen9am8AuGwlxxJtOkuY4m+4MPLvDPOgavKHLsrRuNLAfCeakMlCQ"
                }
              }
            }
          },
          "self_signing_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "self_signing"
              ],
              "keys": {
                "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "AC7oDUW4rUhtInwb4lAoBJ0wAuu4a5k+8e34B5+NKsDB8HXRwgVwUWN/MRWc/sJgtSbVlhzqS9THEmQQ1C51Bw"
                }
              }
            }
          },
          "user_signing_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "user_signing"
              ],
              "keys": {
                "ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo": "DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "C4L2sx9frGqj8w41KyynHGqwUbbwBYRZpYCB+6QWnvQFA5Oi/1PJj8w5anwzEsoO0TWmLYmf7FXuAGewanOWDg"
                }
              }
            }
          }
        }));
        KeyQueryResponse::try_from(data).expect("Can't parse the keys upload response")
    }

    fn device(response: &KeyQueryResponse) -> (ReadOnlyDevice, ReadOnlyDevice) {
        let mut devices = response.device_keys.values().next().unwrap().values();
        let first = ReadOnlyDevice::try_from(devices.next().unwrap()).unwrap();
        let second = ReadOnlyDevice::try_from(devices.next().unwrap()).unwrap();
        (first, second)
    }

    fn own_identity(response: &KeyQueryResponse) -> OwnUserIdentity {
        let user_id = user_id!("@example:localhost");

        let master_key = response.master_keys.get(&user_id).unwrap();
        let user_signing = response.user_signing_keys.get(&user_id).unwrap();
        let self_signing = response.self_signing_keys.get(&user_id).unwrap();

        OwnUserIdentity::new(master_key.into(), self_signing.into(), user_signing.into()).unwrap()
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

    #[test]
    fn own_identity_check_signatures() {
        let response = own_key_query();
        let identity = own_identity(&response);
        let (first, second) = device(&response);

        assert!(identity.is_device_signed(&first).is_err());
        assert!(identity.is_device_signed(&second).is_ok());

        let verification_machine = VerificationMachine::new(
            Account::new(second.user_id(), second.device_id()),
            Arc::new(Box::new(MemoryStore::new())),
        );

        let first = Device {
            inner: first,
            verification_machine: verification_machine.clone(),
            own_identity: Some(identity.clone()),
            device_owner_identity: Some(UserIdentities::Own(identity.clone())),
        };

        let second = Device {
            inner: second,
            verification_machine,
            own_identity: Some(identity.clone()),
            device_owner_identity: Some(UserIdentities::Own(identity.clone())),
        };

        assert!(!second.trust_state());
        assert!(!second.is_trusted());

        assert!(!first.trust_state());
        assert!(!first.is_trusted());

        identity.mark_as_verified();
        assert!(second.trust_state());
        assert!(!first.trust_state());
    }
}
