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

//! The [`Subscriber`] implementation.

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Weak},
};

use tokio::sync::{broadcast::Receiver, mpsc};
use tracing::{trace, warn};

use super::super::AutoShrinkChannelPayload;

/// A structure that can generate handles for subscribers, and count them. See
/// [`SubscriberHandle`] to learn more.
#[derive(Default)]
pub struct SubscribersHandle(Arc<()>);

impl SubscribersHandle {
    /// Count the number of subscribers.
    ///
    /// Similar to [`SubscriberHandle::count`].
    pub fn count(&self) -> usize {
        Arc::weak_count(&self.0)
    }

    /// Generate a handle for a subscriber.
    pub fn new_subscriber_handle(&self) -> SubscriberHandle {
        SubscriberHandle(Arc::downgrade(&self.0))
    }
}

/// A handle for a subscriber.
///
/// A cache might need to track/count the number of subscribers. When the number
/// of subscribers reach zero, it means no more code is listening to it. That's
/// an opportunity to free some memory by shrinking the cache for example!
/// Shrinking means forgetting about “old” events, keeping events from the last
/// chunk for example.
///
/// Every time a subscriber is created,
/// [`SubscribersHandle::new_subscriber_handle`] is called. The resulting value
/// must be kept by the subscriber for its entire lifetime.
pub struct SubscriberHandle(Weak<()>);

impl SubscriberHandle {
    /// Count the number of subscribers.
    ///
    /// Similar to [`SubscribersHandle::count`].
    pub fn count(&self) -> usize {
        Weak::weak_count(&self.0)
    }
}

/// Thin wrapper for a cache subscriber, so as to trigger side-effects when all
/// subscribers are gone.
///
/// The current side-effect is: auto-shrinking the cache when no more
/// subscribers are active. This is an optimisation to reduce the number of data
/// held in memory by the cache: when no more subscribers are active, all data
/// are reduced to the minimum.
///
/// The side-effect takes effect on `Drop`.
#[allow(missing_debug_implementations)]
pub struct Subscriber<T> {
    /// Underlying receiver of the room event cache's updates.
    recv: Receiver<T>,

    /// The payload that is going to be sent to [`Self::auto_shrink_sender`].
    ///
    /// This is an `Option` to take/own the value in the `Drop` implementation
    /// without cloning it.
    auto_shrink_payload: Option<AutoShrinkChannelPayload>,

    /// The sender side of the auto-shrink channel.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// The subscribers handle shared by all subscribers.
    ///
    /// This is used to detect when no more subscribers are active, and trigger
    /// side-effect.
    ///
    /// This is an `Option` to take/own the value in the `Drop` implementation
    /// without cloning it. Being able to own it helps to control when the value
    /// is dropped.
    subscriber_handle: Option<SubscriberHandle>,
}

impl<T> Subscriber<T> {
    /// Create a new [`Subscriber`].
    pub(super) fn new(
        recv: Receiver<T>,
        auto_shrink_payload: AutoShrinkChannelPayload,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        subscribers_handle: &SubscribersHandle,
    ) -> Self {
        Self {
            recv,
            auto_shrink_payload: Some(auto_shrink_payload),
            auto_shrink_sender,
            subscriber_handle: Some(subscribers_handle.new_subscriber_handle()),
        }
    }
}

impl<T> Drop for Subscriber<T> {
    fn drop(&mut self) {
        let number_of_subscribers = self
            .subscriber_handle
            // Ensure the handle is dropped before sending the message to the
            // auto shrink task.
            .take()
            .expect("Unreachable: `subscriber_handle` must be `Some`")
            .count();

        trace!("dropping a room event cache subscriber; count: {number_of_subscribers}");

        if number_of_subscribers == 1 {
            // We were the last instance of the subscriber; let the auto-shrinker know by
            // notifying it.

            // Try to send without waiting for channel capacity, and restart in a loop if it
            // failed (until a maximum number of attempts is reached, or the send was
            // successful). The channel shouldn't be super busy in general, so this should
            // resolve quickly enough.

            let mut payload = self
                .auto_shrink_payload
                .take()
                .expect("Unreachable: `auto_shrink_payload` must be `Some`");
            let mut num_attempts = 0;

            while let Err(err) = self.auto_shrink_sender.try_send(payload) {
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
                    mpsc::error::TrySendError::Full(stolen_payload) => {
                        payload = stolen_payload;
                    }
                    mpsc::error::TrySendError::Closed(_) => return,
                }
            }

            trace!("sent notification to the parent channel that we were the last subscriber");
        }
    }
}

impl<T> Deref for Subscriber<T> {
    type Target = Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.recv
    }
}

impl<T> DerefMut for Subscriber<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.recv
    }
}

#[cfg(test)]
mod tests {
    use super::SubscribersHandle;

    #[test]
    fn test_subscribers_handle() {
        let subscribers_handle = SubscribersHandle::default();

        // No subscribers.
        assert_eq!(subscribers_handle.count(), 0);

        // Pretend a new subscriber exists!
        let handle0 = subscribers_handle.new_subscriber_handle();
        assert_eq!(subscribers_handle.count(), 1);
        assert_eq!(handle0.count(), 1);

        // Pretend another new subscriber exists!
        let handle1 = subscribers_handle.new_subscriber_handle();
        assert_eq!(subscribers_handle.count(), 2);
        assert_eq!(handle0.count(), 2);
        assert_eq!(handle1.count(), 2);

        // A subscriber dies. RIP.
        drop(handle0);
        assert_eq!(subscribers_handle.count(), 1);
        assert_eq!(handle1.count(), 1);

        // Another subscriber dies. RIP.
        drop(handle1);
        assert_eq!(subscribers_handle.count(), 0);

        // Create a new subscriber, for the last test.
        let handle2 = subscribers_handle.new_subscriber_handle();

        // We can even drop the `SubscribersHandle`!
        drop(subscribers_handle);
        assert_eq!(handle2.count(), 0);
        // ZERO, yes, not 1.
        // If the state containing the `SubscribersHandle` drops, there is no
        // more update, and no auto-shrink, so it's fine to get a zero here.
    }
}
