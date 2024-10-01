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

use std::{collections::BTreeMap, mem};

use ruma::{
    events::{AnyGlobalAccountDataEvent, GlobalAccountDataEventType},
    serde::Raw,
};
use tracing::warn;

use crate::StateChanges;

#[must_use]
pub(crate) struct AccountDataProcessor {
    parsed_events: Vec<AnyGlobalAccountDataEvent>,
    raw_by_type: BTreeMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>,
}

impl AccountDataProcessor {
    /// Creates a new processor for global account data.
    pub fn process(events: &[Raw<AnyGlobalAccountDataEvent>]) -> Self {
        let mut raw_by_type = BTreeMap::new();
        let mut parsed_events = Vec::new();

        for raw_event in events {
            let event = match raw_event.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    let event_type: Option<String> = raw_event.get_field("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize a global account data event: {e}");
                    continue;
                }
            };

            raw_by_type.insert(event.event_type(), raw_event.clone());
            parsed_events.push(event);
        }

        Self { raw_by_type, parsed_events }
    }

    /// Applies the processed data to the state changes.
    pub fn apply(mut self, changes: &mut StateChanges) -> Vec<AnyGlobalAccountDataEvent> {
        mem::swap(&mut changes.account_data, &mut self.raw_by_type);
        self.parsed_events
    }
}
