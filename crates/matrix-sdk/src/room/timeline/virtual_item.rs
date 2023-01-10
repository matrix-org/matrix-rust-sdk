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

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(Clone, Debug)]
pub enum VirtualTimelineItem {
    /// A divider between messages of two days.
    DayDivider {
        /// The year.
        year: i32,
        /// The month of the year.
        ///
        /// A value between 1 and 12.
        month: u32,
        /// The day of the month.
        ///
        /// A value between 1 and 31.
        day: u32,
    },

    /// The user's own read marker.
    ReadMarker,

    /// A loading indicator for a pagination request.
    LoadingIndicator,

    /// The beginning of the visible timeline.
    ///
    /// There might be earlier events the user is not allowed to see due to
    /// history visibility.
    TimelineStart,
}
