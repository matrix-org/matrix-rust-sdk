// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use eyeball_im::VectorDiff;
use matrix_sdk_base::event_cache::Event;

use crate::event_cache::EventsOrigin;

pub(super) mod lock;

/// A diff update for an event cache timeline represented as a vector.
#[derive(Clone, Debug)]
pub struct TimelineVectorDiffs {
    /// New vector diff for the thread timeline.
    pub diffs: Vec<VectorDiff<Event>>,
    /// The origin that triggered this update.
    pub origin: EventsOrigin,
}
