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

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

mod inbound;
mod outbound;

pub use inbound::{InboundGroupSession, InboundGroupSessionPickle, PickledInboundGroupSession};
pub use outbound::{EncryptionSettings, OutboundGroupSession};

/// The private session key of a group session.
/// Can be used to create a new inbound group session.
#[derive(Clone, Debug, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct GroupSessionKey(pub String);

#[cfg(test)]
mod test {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use matrix_sdk_common::{
        events::{
            room::message::{MessageEventContent, TextMessageEventContent},
            AnyMessageEventContent,
        },
        identifiers::{room_id, user_id},
    };

    use super::EncryptionSettings;
    use crate::Account;

    #[tokio::test]
    #[cfg(not(target_os = "macos"))]
    async fn expiration() {
        let settings = EncryptionSettings {
            rotation_period_msgs: 1,
            ..Default::default()
        };

        let account = Account::new(&user_id!("@alice:example.org"), "DEVICEID".into());
        let (session, _) = account
            .create_group_session_pair(&room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        let _ = session
            .encrypt(AnyMessageEventContent::RoomMessage(
                MessageEventContent::Text(TextMessageEventContent::plain("Test message")),
            ))
            .await;
        assert!(session.expired());

        let settings = EncryptionSettings {
            rotation_period: Duration::from_millis(100),
            ..Default::default()
        };

        let (mut session, _) = account
            .create_group_session_pair(&room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        session.creation_time = Arc::new(Instant::now() - Duration::from_secs(60 * 60));
        assert!(session.expired());
    }
}
