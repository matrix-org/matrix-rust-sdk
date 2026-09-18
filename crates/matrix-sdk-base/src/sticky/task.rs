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

//! The background task expiring the sticky events of a room.
//!
//! Readers of the map filter out expired entries themselves, so this task is
//! only there to broadcast expiries to subscribers as they happen (rather than
//! at the next sync), and to keep the map from holding onto dead entries.

use std::{pin::pin, sync::Arc, time::Duration};

use futures_util::future::select;
use matrix_sdk_common::{
    executor::{self, AbortOnDrop, JoinHandleExt as _},
    sleep::sleep,
};
use tokio::sync::Notify;

use super::{WeakStickyEvents, now_ms};

/// The longest the task sleeps before re-evaluating the expiries.
///
/// The sleep is measured by a monotonic clock while expiries are wall-clock
/// times; the two drift apart when the device sleeps. Capping the sleep bounds
/// how late an expiry can be noticed after the device wakes up.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Spawn the task for the sticky events behind `inner`.
///
/// The task holds a weak reference only, and stops as soon as the sticky
/// events are dropped; the returned guard aborts it right away in that case.
pub(super) fn spawn(inner: WeakStickyEvents, changed: Arc<Notify>) -> AbortOnDrop<()> {
    executor::spawn(run(inner, changed)).abort_on_drop()
}

async fn run(inner: WeakStickyEvents, changed: Arc<Notify>) {
    loop {
        let Some(sticky) = inner.upgrade() else {
            break;
        };

        let now = now_ms();
        let next_expiry = sticky.expire(now);

        // Don't keep the sticky events alive while waiting.
        drop(sticky);

        let notified = pin!(changed.notified());

        match next_expiry {
            // Nothing to expire: wait for something to be added.
            None => notified.await,
            // Sleep until the next expiry, unless something changes before.
            Some(next_expiry) => {
                let until_expiry = Duration::from_millis(next_expiry.saturating_sub(now));
                let _ = select(pin!(sleep(until_expiry.min(MAX_SLEEP))), notified).await;
            }
        }
    }
}
