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

//! A simple algorithm that assumes that there are segments that are denoted by
//! `start` and `end` markers. The algorithm begins accepts a "starting point"
//! (the end mark of a segment) with its corresponding position. It then expects
//! that a user provides marks one-by-one with their corresponding position
//! (going backwards), and returns once the start mark of all provided segments
//! are found, i.e. essentially a start mark of a "super-segment" that contains
//! all of the segments that were fed to the algorithm.
//!
//! The algorithm is used to find the start of a call in a room, where each call
//! membership (each participant) can be viewed as a segment denoted by two
//! state member events, each of which may be a "start" (join) and "end" (left)
//! points. The "position" in this case would be a timestamp of an event.

use std::{collections::HashSet, hash::Hash};

/// Marks segment start or end.
pub(super) struct Mark<Id, Pos> {
    pub id: Id,
    pub position: Pos,
    pub kind: Kind,
}

/// Type of a segment.
pub(super) enum Kind {
    Start,
    End,
}

impl<Id, Pos> Mark<Id, Pos> {
    pub(super) fn start(id: Id, position: Pos) -> Self {
        let kind = Kind::Start;
        Self { id, position, kind }
    }

    pub(super) fn end(id: Id, position: Pos) -> Self {
        let kind = Kind::End;
        Self { id, position, kind }
    }
}

/// Tiny state machine that tries to find the beginning (start) of a segment.
pub(super) struct FindStart<Id, Pos> {
    /// End position.
    end: Pos,
    /// Currently known start position.
    start: Option<Pos>,
    /// Start markers that we're waiting for.
    wait_for_start_mark: HashSet<Id>,
}

impl<Id: Eq + Hash, Pos> FindStart<Id, Pos> {
    /// Creates a new state machine with a given reference point that we start
    /// our search from (we go backwards from this mark's position).
    pub(super) fn new(ref_id: impl IntoIterator<Item = Id>, ref_pos: Pos) -> Self {
        Self { start: None, end: ref_pos, wait_for_start_mark: HashSet::from_iter(ref_id) }
    }
}

/// The result of processing the next mark.
pub(super) enum FindState<Id, Pos> {
    /// We're still in a progress of finding a beginning of a segment.
    InProgress(FindStart<Id, Pos>),
    Completed {
        /// Start point of a segment.
        start: Pos,
        /// End point of a segment.
        end: Pos,
        /// Next segment state machine.
        next: FindStart<Id, Pos>,
    },
}

impl<Id: Eq + Hash, Pos: Ord> FindStart<Id, Pos> {
    /// Pass the next mark to the state machine. The marks should go "backwards"
    /// in `position`. Returns a completed segment if the segment's start
    /// position is found.
    pub(super) fn process(mut self, mark: Mark<Id, Pos>) -> FindState<Id, Pos> {
        // If we receive a mark that is after the end of the segment, we ignore it.
        if mark.position > self.end {
            return FindState::InProgress(self);
        }

        match mark.kind {
            Kind::Start => {
                let cmp = |p| &mark.position < p;
                if self.start.as_ref().map(cmp).unwrap_or(true) {
                    self.start = Some(mark.position);
                }
                self.wait_for_start_mark.remove(&mark.id);
            }
            Kind::End => {
                if self.wait_for_start_mark.is_empty()
                    && matches!(self.start, Some(ref start) if &mark.position < start)
                {
                    return FindState::Completed {
                        start: self.start.unwrap(),
                        end: self.end,
                        next: Self::new([mark.id], mark.position),
                    };
                }

                self.wait_for_start_mark.insert(mark.id);
            }
        }

        FindState::InProgress(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{FindStart, FindState, Mark};

    #[test]
    fn call_duration() {
        // Start counting from "now" (ts=150). There are 2 participants who in a call
        // (p1, p2).
        let search = FindStart::new(["p1", "p2"], 150);

        // A participant who left recently (p3).
        let segment = search.process(Mark::end("p3", 120));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // Here is the start event of p1.
        let segment = search.process(Mark::start("p1", 100));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // The start event of p2.
        let segment = search.process(Mark::start("p2", 80));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // A participant who was in a call before p1 and p2 (p4).
        let segment = search.process(Mark::end("p4", 70));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // A participant who left the call while p1 and p2 were there (p3) start event.
        let segment = search.process(Mark::start("p3", 60));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // A participant who was earliest in the call (p4) start event.
        let segment = search.process(Mark::start("p4", 50));
        let FindState::InProgress(search) = segment else {
            panic!("Expected in progress segment");
        };

        // A participant who was there in a different call before.
        let segment = search.process(Mark::end("p5", 40));
        let FindState::Completed { start, end, .. } = segment else {
            panic!("Expected completed segment");
        };

        // Start = 50, end = 150. Duration is 100.
        assert!(start == 50);
        assert!(end == 150);
    }
}
