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

//! The [`RoomEventCacheSubscriber`] implementation.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ruma::OwnedRoomId;
use tokio::sync::{broadcast::Receiver, mpsc};
use tracing::{trace, warn};

use super::{AutoShrinkChannelPayload, RoomEventCacheUpdate};

/// Thin wrapper for a room event cache subscriber, so as to trigger
/// side-effects when all subscribers are gone.
///
/// The current side-effect is: auto-shrinking the [`RoomEventCache`] when no
/// more subscribers are active. This is an optimisation to reduce the number of
/// data held in memory by a [`RoomEventCache`]: when no more subscribers are
/// active, all data are reduced to the minimum.
///
/// The side-effect takes effect on `Drop`.
#[allow(missing_debug_implementations)]
pub struct RoomEventCacheSubscriber {
    /// Underlying receiver of the room event cache's updates.
    recv: Receiver<RoomEventCacheUpdate>,

    /// To which room are we listening?
    room_id: OwnedRoomId,

    /// Sender to the auto-shrink channel.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// Shared instance of the auto-shrinker.
    subscriber_count: Arc<AtomicUsize>,
}

impl RoomEventCacheSubscriber {
    /// Create a new [`RoomEventCacheSubscriber`].
    pub(super) fn new(
        recv: Receiver<RoomEventCacheUpdate>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        subscriber_count: Arc<AtomicUsize>,
    ) -> Self {
        Self { recv, room_id, auto_shrink_sender, subscriber_count }
    }
}

impl Drop for RoomEventCacheSubscriber {
    fn drop(&mut self) {
        let previous_subscriber_count = self.subscriber_count.fetch_sub(1, Ordering::SeqCst);

        trace!(
            "dropping a room event cache subscriber; previous count: {previous_subscriber_count}"
        );

        if previous_subscriber_count == 1 {
            // We were the last instance of the subscriber; let the auto-shrinker know by
            // notifying it of our room id.

            let mut room_id = self.room_id.clone();

            // Try to send without waiting for channel capacity, and restart in a spin-loop
            // if it failed (until a maximum number of attempts is reached, or
            // the send was successful). The channel shouldn't be super busy in
            // general, so this should resolve quickly enough.

            let mut num_attempts = 0;

            while let Err(err) = self.auto_shrink_sender.try_send(room_id) {
                num_attempts += 1;

                if num_attempts > 1024 {
                    // If we've tried too many times, just give up with a warning; after all, this
                    // is only an optimization.
                    warn!(
                        "couldn't send notification to the auto-shrink channel \
                         after 1024 attempts; giving up"
                    );
                    return;
                }

                match err {
                    mpsc::error::TrySendError::Full(stolen_room_id) => {
                        room_id = stolen_room_id;
                    }
                    mpsc::error::TrySendError::Closed(_) => return,
                }
            }

            trace!("sent notification to the parent channel that we were the last subscriber");
        }
    }
}

impl Deref for RoomEventCacheSubscriber {
    type Target = Receiver<RoomEventCacheUpdate>;

    fn deref(&self) -> &Self::Target {
        &self.recv
    }
}

impl DerefMut for RoomEventCacheSubscriber {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.recv
    }
}
