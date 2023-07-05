// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use ruma::MilliSecondsSinceUnixEpoch;

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(Clone, Debug)]
pub enum VirtualTimelineItem {
    /// A divider between messages of two days.
    ///
    /// The value is a timestamp in milliseconds since Unix Epoch on the given
    /// day in local time.
    DayDivider(MilliSecondsSinceUnixEpoch),

    /// The user's own read marker.
    ReadMarker,
}
