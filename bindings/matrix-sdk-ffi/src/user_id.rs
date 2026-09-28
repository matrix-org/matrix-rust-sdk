// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

use ruma::UserId;

use crate::error::ClientError;

/// The server name part of the given user ID, including the port when the
/// server name has one.
///
/// Returns an error if the user ID is invalid.
#[matrix_sdk_ffi_macros::export]
pub fn server_name_from_user_id(user_id: String) -> Result<String, ClientError> {
    let user_id = UserId::parse(user_id)?;
    Ok(user_id.server_name().to_string())
}

#[cfg(test)]
mod tests {
    use super::server_name_from_user_id;

    #[test]
    fn test_server_name_from_user_id() {
        assert_eq!(
            server_name_from_user_id("@alice:example.org".to_owned()).unwrap(),
            "example.org"
        );
    }

    #[test]
    fn test_server_name_from_user_id_keeps_the_port() {
        assert_eq!(
            server_name_from_user_id("@alice:example.org:8448".to_owned()).unwrap(),
            "example.org:8448"
        );
    }

    #[test]
    fn test_server_name_from_user_id_with_an_invalid_user_id() {
        assert!(server_name_from_user_id("example.org".to_owned()).is_err());
        assert!(server_name_from_user_id("@alice".to_owned()).is_err());
    }
}
