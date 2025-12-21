// Copyright 2021 Devin Ragotzy.
// Copyright 2021 Timo Kösters.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

mod common;
mod master;
mod self_signing;
mod user_signing;

pub use common::*;
pub use master::*;
pub use self_signing::*;
pub use user_signing::*;

macro_rules! impl_partial_eq {
    ($key_type: ty) => {
        impl PartialEq for $key_type {
            /// The `PartialEq` implementation compares the user ID, the usage and the
            /// key material, ignoring signatures.
            ///
            /// The usage could be safely ignored since the type guarantees it has the
            /// correct usage by construction -- it is impossible to construct a
            /// value of a particular key type with an incorrect usage. However, we
            /// check it anyway, to codify the notion that the same key material
            /// with a different usage results in a logically different key.
            ///
            /// The signatures are provided by other devices and don't alter the
            /// identity of the key itself.
            fn eq(&self, other: &Self) -> bool {
                self.user_id() == other.user_id()
                    && self.keys() == other.keys()
                    && self.usage() == other.usage()
            }
        }
        impl Eq for $key_type {}
    };
}

impl_partial_eq!(MasterPubkey);
impl_partial_eq!(SelfSigningPubkey);
impl_partial_eq!(UserSigningPubkey);

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{DeviceKeyId, encryption::KeyUsage, user_id};
    use serde_json::json;
    use vodozemac::Ed25519Signature;

    use crate::{
        identities::{
            manager::testing::{own_key_query, own_key_query_with_user_id},
            user::testing::get_other_own_identity,
        },
        types::{CrossSigningKey, MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
    };

    #[test]
    fn serialization() {
        let json = json!({
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
              },
              "other_data": "other"
        });

        let key: CrossSigningKey =
            serde_json::from_value(json.clone()).expect("Can't deserialize cross signing key");

        assert_eq!(key.user_id, user_id!("@example:localhost"));

        let serialized = serde_json::to_value(key).expect("Can't reserialize cross signing key");

        assert_eq!(json, serialized);
    }

    #[async_test]
    async fn test_partial_eq_cross_signing_keys() {
        macro_rules! test_partial_eq {
            ($key_type:ident, $key_field:ident, $field:ident, $usage:expr) => {
                let user_id = user_id!("@example:localhost");
                let response = own_key_query();
                let raw = response.$field.get(user_id).unwrap();
                let key: $key_type = raw.deserialize_as_unchecked().unwrap();

                // A different key is naturally not the same as our key.
                let other_identity = get_other_own_identity().await;
                let other_key = other_identity.$key_field();
                assert_ne!(&key, other_key);

                // However, not even our own key material with another user ID is the same.
                let other_user_id = user_id!("@example2:localhost");
                let other_response = own_key_query_with_user_id(&other_user_id);
                let other_raw = other_response.$field.get(other_user_id).unwrap();
                let other_key: $key_type = other_raw.deserialize_as_unchecked().unwrap();
                assert_ne!(key, other_key);

                // Now let's add another signature to our key.
                let signature = Ed25519Signature::from_base64(
                    "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
                ).expect("The signature can always be decoded");
                let mut other_key: CrossSigningKey = raw.deserialize_as().unwrap();
                other_key.signatures.add_signature(
                    user_id.to_owned(),
                    DeviceKeyId::from_parts(ruma::DeviceKeyAlgorithm::Ed25519, "DEVICEID".into()),
                    signature,
                );
                let other_key = other_key.try_into().unwrap();

                // Additional signatures are fine, adding more does not change the key's identity.
                assert_eq!(key, other_key);

                // However changing the usage results in a different key.
                let mut other_key: CrossSigningKey = raw.deserialize_as().unwrap();
                other_key.usage.push($usage);
                let other_key = $key_type { 0: other_key.into() };
                assert_ne!(key, other_key);
            };
        }

        // The last argument is deliberately some usage which is *not* correct for the
        // type.
        test_partial_eq!(MasterPubkey, master_key, master_keys, KeyUsage::SelfSigning);
        test_partial_eq!(SelfSigningPubkey, self_signing_key, self_signing_keys, KeyUsage::Master);
        test_partial_eq!(UserSigningPubkey, user_signing_key, user_signing_keys, KeyUsage::Master);
    }
}
