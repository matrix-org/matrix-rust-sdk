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

use tokio::sync::broadcast::{Receiver, Sender};

use super::super::{super::RoomEventCacheGenericUpdate, TimelineVectorDiffs};

/// A small type to send updates in all channels.
#[derive(Clone)]
pub struct ThreadEventCacheUpdateSender {
    thread_sender: Sender<TimelineVectorDiffs>,
    generic_sender: Sender<RoomEventCacheGenericUpdate>,
}

impl ThreadEventCacheUpdateSender {
    /// Create a new [`ThreadEventCacheUpdateSender`].
    pub fn new(generic_sender: Sender<RoomEventCacheGenericUpdate>) -> Self {
        Self { thread_sender: Sender::new(32), generic_sender }
    }

    /// Send a [`TimelineVectorDiffs`].
    pub fn send(
        &self,
        thread_update: TimelineVectorDiffs,
        generic_update: Option<RoomEventCacheGenericUpdate>,
    ) {
        let _ = self.thread_sender.send(thread_update);

        if let Some(generic_update) = generic_update {
            let _ = self.generic_sender.send(generic_update);
        }
    }

    /// Create a new [`Receiver`] of [`TimelineVectorDiffs`].
    pub(super) fn new_thread_receiver(&self) -> Receiver<TimelineVectorDiffs> {
        self.thread_sender.subscribe()
    }
}
