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

use ruma::{
    events::{AnyTimelineEvent, TimelineEventType},
    serde::Raw,
    OwnedEventId,
};
use serde::{Deserialize, Serialize};

use crate::widget::client::actions::SendEventCommand;

/// Various actions that we may ask the caller to perform after
/// handling a request and sending a response.
pub(crate) enum PostAction {
    NegotiatePermissions,
    UpdateOpenId(String),
}

/// A simple proxy to perform Matrix-related actions.
#[async_trait::async_trait]
pub(crate) trait MatrixProxy: Send {
    async fn send(&self, req: SendEventCommand) -> Result<OwnedEventId, String>;
    async fn read(&self, req: ReadEventBody) -> Result<Vec<Raw<AnyTimelineEvent>>, String>;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReadEventBody {
    #[serde(rename = "type")]
    pub(crate) event_type: TimelineEventType,
    pub(crate) state_key: Option<String>,
    pub(crate) limit: Option<u32>,
}
