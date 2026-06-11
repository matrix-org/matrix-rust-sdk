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
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk::{RumaApiError, encryption::vodozemac::pk_encryption::PkEncryption};
use matrix_sdk_crypto::vodozemac::Curve25519PublicKey;
use ruma::{
    api::{
        EndpointError, IncomingResponse,
        error::{FromHttpResponseError, IntoHttpError},
    },
    events::room::EncryptedFile,
    exports::{http::Response, serde_json},
};
use serde::Deserialize;

use crate::EncryptedFileRequest;

pub mod download;
pub mod public_server_key;
pub mod scan;

/// The HTTP response content for a successful request to download and scan
/// media.
/// Spec: <https://github.com/element-hq/matrix-content-scanner-python/blob/main/docs/api.md>
#[derive(Debug, Deserialize)]
pub struct DownloadAndScanMediaResponse {
    /// The content that was previously uploaded.
    pub content: Vec<u8>,
}

impl IncomingResponse for DownloadAndScanMediaResponse {
    type EndpointError = RumaApiError;

    fn try_from_http_response<T: AsRef<[u8]>>(
        response: Response<T>,
    ) -> Result<Self, FromHttpResponseError<Self::EndpointError>> {
        if response.status().is_success() {
            Ok(Self { content: response.body().as_ref().to_vec() })
        } else {
            Err(FromHttpResponseError::Server(Self::EndpointError::from_http_response(response)))
        }
    }
}

/// Creates an [`EncryptedFileRequest`] from [`EncryptedFile`], using the
/// optionally provided public key if present.
pub(crate) fn encrypted_file_request_from(
    public_key: Option<Curve25519PublicKey>,
    encrypted_file: &EncryptedFile,
) -> Result<EncryptedFileRequest, IntoHttpError> {
    if let Some(public_key) = public_key {
        // Generate an encrypted request body.
        let encryption =
            PkEncryption::from_key(Curve25519PublicKey::from_bytes(*public_key.as_bytes()));

        let body_to_encrypt = EncryptedFileRequest::from_file_info(encrypted_file.clone());
        let json_body_to_encrypt = serde_json::to_string(&body_to_encrypt)?;
        let pk_message = encryption
            .encrypt(json_body_to_encrypt.as_bytes())
            .map_err(|e| IntoHttpError::Authentication(e.into()))?;
        Ok(EncryptedFileRequest::from_encrypted_body(pk_message.into()))
    } else {
        Ok(EncryptedFileRequest::from_file_info(encrypted_file.clone()))
    }
}
