// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeSet, sync::Arc};

use extension_trait::extension_trait;
use ruma::{
    events::room::{message::RoomMessageEventContentWithoutRelation, MediaSource},
    OwnedUserId, UserId,
};

use crate::{error::ClientError, helpers::unwrap_or_clone_arc};

#[extension_trait]
pub impl MediaSourceExt for MediaSource {
    fn from_json(json: String) -> Result<MediaSource, ClientError> {
        let res = serde_json::from_str(&json)?;
        Ok(res)
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Media source should always be serializable ")
    }

    fn url(&self) -> String {
        match self {
            MediaSource::Plain(url) => url.to_string(),
            MediaSource::Encrypted(file) => file.url.to_string(),
        }
    }
}

#[extension_trait]
pub impl RoomMessageEventContentWithoutRelationExt for RoomMessageEventContentWithoutRelation {
    fn with_mentions(self: Arc<Self>, mentions: Mentions) -> Arc<Self> {
        let mut content = unwrap_or_clone_arc(self);
        content.mentions = Some(mentions.into());
        Arc::new(content)
    }
}

pub struct Mentions {
    pub user_ids: Vec<String>,
    pub room: bool,
}

impl From<Mentions> for ruma::events::Mentions {
    fn from(value: Mentions) -> Self {
        let mut user_ids = BTreeSet::<OwnedUserId>::new();
        for user_id in value.user_ids {
            if let Ok(user_id) = UserId::parse(user_id) {
                user_ids.insert(user_id);
            }
        }
        let mut result = Self::default();
        result.user_ids = user_ids;
        result.room = value.room;
        result
    }
}
