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

use std::{fmt::Debug, sync::Arc, time::Duration};

use matrix_sdk_base::crypto::types::events::UtdCause;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::unable_to_decrypt_hook::{
    UnableToDecryptHook, UnableToDecryptInfo as SdkUnableToDecryptInfo,
};

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait UnableToDecryptDelegate: SyncOutsideWasm + SendOutsideWasm {
    fn on_utd(&self, info: UnableToDecryptInfo);
}

pub struct UtdHook {
    pub delegate: Arc<dyn UnableToDecryptDelegate>,
}

impl std::fmt::Debug for UtdHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UtdHook").finish_non_exhaustive()
    }
}

impl UnableToDecryptHook for UtdHook {
    fn on_utd(&self, info: SdkUnableToDecryptInfo) {
        const IGNORE_UTD_PERIOD: Duration = Duration::from_secs(4);

        // UTDs that have been decrypted in the `IGNORE_UTD_PERIOD` are just ignored and
        // not considered UTDs.
        if let Some(duration) = &info.time_to_decrypt {
            if *duration < IGNORE_UTD_PERIOD {
                return;
            }
        }

        // Report the UTD to the client.
        self.delegate.on_utd(info.into());
    }
}

#[derive(uniffi::Record)]
pub struct UnableToDecryptInfo {
    /// The identifier of the event that couldn't get decrypted.
    event_id: String,

    /// If the event could be decrypted late (that is, the event was encrypted
    /// at first, but could be decrypted later on), then this indicates the
    /// time it took to decrypt the event. If it is not set, this is
    /// considered a definite UTD.
    ///
    /// If set, this is in milliseconds.
    pub time_to_decrypt_ms: Option<u64>,

    /// What we know about what caused this UTD. E.g. was this event sent when
    /// we were not a member of this room?
    pub cause: UtdCause,

    /// The difference between the event creation time (`origin_server_ts`) and
    /// the time our device was created. If negative, this event was sent
    /// *before* our device was created.
    pub event_local_age_millis: i64,

    /// Whether the user had verified their own identity at the point they
    /// received the UTD event.
    pub user_trusts_own_identity: bool,

    /// The homeserver of the user that sent the undecryptable event.
    pub sender_homeserver: String,

    /// Our local user's own homeserver, or `None` if the client is not logged
    /// in.
    pub own_homeserver: Option<String>,
}

impl From<SdkUnableToDecryptInfo> for UnableToDecryptInfo {
    fn from(value: SdkUnableToDecryptInfo) -> Self {
        Self {
            event_id: value.event_id.to_string(),
            time_to_decrypt_ms: value.time_to_decrypt.map(|ttd| ttd.as_millis() as u64),
            cause: value.cause,
            event_local_age_millis: value.event_local_age_millis,
            user_trusts_own_identity: value.user_trusts_own_identity,
            sender_homeserver: value.sender_homeserver.to_string(),
            own_homeserver: value.own_homeserver.map(String::from),
        }
    }
}
