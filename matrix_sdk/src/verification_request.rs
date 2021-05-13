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

use matrix_sdk_base::crypto::{
    OutgoingVerificationRequest, VerificationRequest as BaseVerificationRequest,
};

use crate::{Client, Result};

/// An object controling the interactive verification flow.
#[derive(Debug, Clone)]
pub struct VerificationRequest {
    pub(crate) inner: BaseVerificationRequest,
    pub(crate) client: Client,
}

impl VerificationRequest {
    /// Accept the verification request
    pub async fn accept(&self) -> Result<()> {
        if let Some(request) = self.inner.accept() {
            match request {
                OutgoingVerificationRequest::ToDevice(r) => {
                    self.client.send_to_device(&r).await?;
                }
                OutgoingVerificationRequest::InRoom(r) => {
                    self.client.room_send_helper(&r).await?;
                }
            };
        }

        Ok(())
    }

    /// Cancel the verification request
    pub async fn cancel(&self) -> Result<()> {
        todo!()
    }
}
