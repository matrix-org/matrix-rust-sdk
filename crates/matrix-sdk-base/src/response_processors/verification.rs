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

use matrix_sdk_crypto::OlmMachine;
use ruma::{events::AnySyncMessageLikeEvent, RoomId};

use super::Context;
use crate::Result;

pub async fn verification(
    _context: &mut Context,
    verification_is_allowed: bool,
    olm_machine: Option<&OlmMachine>,
    event: &AnySyncMessageLikeEvent,
    room_id: &RoomId,
) -> Result<()> {
    if !verification_is_allowed {
        return Ok(());
    }

    if let Some(olm) = olm_machine {
        olm.receive_verification_event(&event.clone().into_full_event(room_id.to_owned())).await?;
    }

    Ok(())
}
