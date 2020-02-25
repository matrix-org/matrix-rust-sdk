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

use std::convert::TryInto;

use super::olm::Account;
use crate::api;

use api::r0::keys;

struct OlmMachine {
    /// The unique user id that owns this account.
    user_id: String,
    /// The unique device id of the device that holds this account.
    device_id: String,
    /// Our underlying Olm Account holding our identity keys.
    account: Account,
    /// The number of one-time keys we have uploaded to the server. If this is
    /// None, no action will be taken. After a sync request the client needs to
    /// set this for us, depending on the count we will suggest the client
    /// to upload new keys.
    uploaded_key_count: Option<u64>,
}

impl OlmMachine {
    /// Create a new account.
    pub fn new(user_id: &str, device_id: &str) -> Self {
        OlmMachine {
            user_id: user_id.to_owned(),
            device_id: device_id.to_owned(),
            account: Account::new(),
            uploaded_key_count: None,
        }
    }

    /// Should account or one-time keys be uploaded to the server.
    pub fn should_upload_keys(&self) -> bool {
        if !self.account.shared() {
            return true;
        }

        // If we have a known key count, check that we have more than
        // max_one_time_Keys() / 2, otherwise tell the client to upload more.
        match self.uploaded_key_count {
            Some(count) => {
                let max_keys = self.account.max_one_time_keys() as u64;
                let key_count = (max_keys / 2) - count;
                key_count > 0
            }
            None => false,
        }
    }

    /// Receive a successfull keys upload response.
    ///
    /// # Arugments
    ///
    /// `response` - The keys upload response of the request that the client
    ///     performed.
    pub async fn receive_keys_upload_response(&mut self, response: &keys::upload_keys::Response) {
        self.account.shared = true;
        let one_time_key_count = response
            .one_time_key_counts
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        if let Some(c) = one_time_key_count {
            let count: u64 = (*c).into();
            self.uploaded_key_count = Some(count);
        }

        self.account.mark_keys_as_published();
        // TODO save the account here.
    }

    /// Generate new one-time keys.
    ///
    /// Returns the number of newly generated one-time keys. If no keys can be
    /// generated returns an empty error.
    fn generate_one_time_keys(&self) -> Result<u64, ()> {
        match self.uploaded_key_count {
            Some(count) => {
                let max_keys = self.account.max_one_time_keys() as u64;
                let key_count = (max_keys / 2) - count;

                if key_count <= 0 {
                    return Err(());
                }

                let key_count: usize = key_count
                    .try_into()
                    .unwrap_or_else(|_| self.account.max_one_time_keys());

                self.account.generate_one_time_keys(key_count);
                Ok(key_count as u64)
            }
            None => Err(()),
        }
    }

    fn device_keys() -> () {}
}

#[cfg(test)]
mod test {
    const USER_ID: &str = "@test:example.org";
    const DEVICE_ID: &str = "DEVICEID";

    use js_int::UInt;
    use std::convert::TryFrom;
    use std::fs::File;
    use std::io::prelude::*;

    use crate::api::r0::keys;
    use crate::crypto::machine::OlmMachine;
    use http::Response;

    fn response_from_file(path: &str) -> Response<Vec<u8>> {
        let mut file = File::open(path).expect(&format!("No such data file found {}", path));
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .expect(&format!("Can't read data file {}", path));

        Response::builder().status(200).body(contents).unwrap()
    }

    fn keys_upload_response() -> keys::upload_keys::Response {
        let data = response_from_file("tests/data/keys_upload.json");
        keys::upload_keys::Response::try_from(data).expect("Can't parse the keys upload response")
    }

    #[test]
    fn create_olm_machine() {
        let machine = OlmMachine::new(USER_ID, DEVICE_ID);
        assert!(machine.should_upload_keys());
    }

    #[async_std::test]
    async fn receive_keys_upload_response() {
        let mut machine = OlmMachine::new(USER_ID, DEVICE_ID);
        let mut response = keys_upload_response();

        response
            .one_time_key_counts
            .remove(&keys::KeyAlgorithm::SignedCurve25519)
            .unwrap();

        assert!(machine.should_upload_keys());
        machine.receive_keys_upload_response(&response).await;
        assert!(!machine.should_upload_keys());

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
        let mut machine = OlmMachine::new(USER_ID, DEVICE_ID);

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
}
