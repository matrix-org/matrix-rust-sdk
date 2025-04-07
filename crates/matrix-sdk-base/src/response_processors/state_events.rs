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

use ruma::{
    events::{AnyStrippedStateEvent, AnySyncStateEvent},
    serde::Raw,
};
use serde::Deserialize;
use tracing::warn;

use super::Context;

pub fn collect_sync(
    _context: &mut Context,
    raw_events: &[Raw<AnySyncStateEvent>],
) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
    collect(raw_events)
}

pub fn collect_stripped(
    _context: &mut Context,
    raw_events: &[Raw<AnyStrippedStateEvent>],
) -> (Vec<Raw<AnyStrippedStateEvent>>, Vec<AnyStrippedStateEvent>) {
    collect(raw_events)
}

fn collect<'a, T>(raw_events: &'a [Raw<T>]) -> (Vec<Raw<T>>, Vec<T>)
where
    T: Deserialize<'a>,
{
    raw_events
        .iter()
        .filter_map(|raw_event| match raw_event.deserialize() {
            Ok(event) => Some((raw_event.clone(), event)),
            Err(e) => {
                warn!("Couldn't deserialize stripped state event: {e}");
                None
            }
        })
        .unzip()
}
