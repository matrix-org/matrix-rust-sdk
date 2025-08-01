// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_ui::spaces::SpaceService as UISpaceService;
use ruma::RoomId;

use crate::error::ClientError;

#[derive(uniffi::Object)]
pub struct SpaceService {
    inner: UISpaceService,
}

impl SpaceService {
    /// Create a new instance of `SpaceService`.
    pub fn new(inner: UISpaceService) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceService {
    /// Get the list of joined spaces.
    pub fn joined_spaces(&self) -> Vec<String> {
        self.inner.joined_spaces().into_iter().map(|id| id.to_string()).collect()
    }

    /// Get the top-level children for a given space.
    pub async fn top_level_children_for(
        &self,
        space_id: String,
    ) -> Result<Vec<String>, ClientError> {
        let space_id = RoomId::parse(space_id)?;
        let children = self.inner.top_level_children_for(space_id).await?;
        Ok(children.into_iter().map(|id| id.to_string()).collect())
    }
}
