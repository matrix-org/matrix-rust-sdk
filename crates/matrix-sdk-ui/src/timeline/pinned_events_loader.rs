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

use matrix_sdk::{BoxFuture, Room, SendOutsideWasm, SyncOutsideWasm, config::RequestConfig};
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use ruma::{EventId, events::relation::RelationType, uint};

pub trait PinnedEventsRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Load a single room event using the cache or network and any events
    /// related to it, if they are cached.
    ///
    /// You can control which types of related events are retrieved using
    /// `related_event_filters`. A `None` value will retrieve any type of
    /// related event.
    fn load_event_with_relations<'a>(
        &'a self,
        event_id: &'a EventId,
        request_config: Option<RequestConfig>,
        related_event_filters: Option<Vec<RelationType>>,
    ) -> BoxFuture<'a, Result<(TimelineEvent, Vec<TimelineEvent>), matrix_sdk::Error>>;
}

impl PinnedEventsRoom for Room {
    fn load_event_with_relations<'a>(
        &'a self,
        event_id: &'a EventId,
        request_config: Option<RequestConfig>,
        related_event_filters: Option<Vec<RelationType>>,
    ) -> BoxFuture<'a, Result<(TimelineEvent, Vec<TimelineEvent>), matrix_sdk::Error>> {
        Box::pin(self.load_or_fetch_event_with_relations(
            event_id,
            related_event_filters,
            request_config,
        ))
    }
}
