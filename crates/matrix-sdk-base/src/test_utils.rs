// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]

use ruma::{UserId, owned_user_id};

use crate::{
    BaseClient, SessionMeta,
    client::ThreadingSupport,
    store::{RoomLoadSettings, StoreConfig},
};

/// Create a [`BaseClient`] with the given user id, if provided, or an hardcoded
/// one otherwise.
pub(crate) async fn logged_in_base_client(user_id: Option<&UserId>) -> BaseClient {
    let client = BaseClient::new(
        StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
        ThreadingSupport::Disabled,
    );
    let user_id =
        user_id.map(|user_id| user_id.to_owned()).unwrap_or_else(|| owned_user_id!("@u:e.uk"));
    client
        .activate(
            SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
            RoomLoadSettings::default(),
            #[cfg(feature = "e2e-encryption")]
            None,
        )
        .await
        .expect("`activate` failed!");
    client
}
