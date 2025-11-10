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

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
};

use matrix_sdk_base::{
    StateStoreDataKey, StateStoreDataValue, ThreadSubscriptionCatchupToken,
    executor::AbortOnDrop,
    store::{StoredThreadSubscription, ThreadSubscriptionStatus},
};
use matrix_sdk_common::executor::spawn;
use once_cell::sync::OnceCell;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, RoomId,
    api::client::threads::get_thread_subscriptions_changes::unstable::{
        ThreadSubscription, ThreadUnsubscription,
    },
    assign,
};
use tokio::sync::{
    Mutex, OwnedMutexGuard,
    mpsc::{Receiver, Sender, channel},
};
use tracing::{instrument, trace, warn};

use crate::{Client, Result, client::WeakClient};

struct GuardedStoreAccess {
    _mutex: OwnedMutexGuard<()>,
    client: Client,
    is_outdated: Arc<AtomicBool>,
}

impl GuardedStoreAccess {
    /// Return the current list of catchup tokens, if any.
    ///
    /// It is guaranteed that if the list is set, then it's non-empty.
    async fn load_catchup_tokens(&self) -> Result<Option<Vec<ThreadSubscriptionCatchupToken>>> {
        let loaded = self
            .client
            .state_store()
            .get_kv_data(StateStoreDataKey::ThreadSubscriptionsCatchupTokens)
            .await?;

        match loaded {
            Some(data) => {
                if let Some(tokens) = data.into_thread_subscriptions_catchup_tokens() {
                    // If the tokens list is empty, automatically clean it up.
                    if tokens.is_empty() {
                        self.save_catchup_tokens(tokens).await?;
                        Ok(None)
                    } else {
                        Ok(Some(tokens))
                    }
                } else {
                    warn!(
                        "invalid data in thread subscriptions catchup tokens state store k/v entry"
                    );
                    Ok(None)
                }
            }

            None => Ok(None),
        }
    }

    /// Saves the tokens in the database.
    ///
    /// Returns whether the list of tokens is empty or not.
    #[instrument(skip_all, fields(num_tokens = tokens.len()))]
    async fn save_catchup_tokens(
        &self,
        tokens: Vec<ThreadSubscriptionCatchupToken>,
    ) -> Result<bool> {
        let store = self.client.state_store();
        let is_empty = if tokens.is_empty() {
            store.remove_kv_data(StateStoreDataKey::ThreadSubscriptionsCatchupTokens).await?;

            trace!("Marking thread subscriptions as not outdated \\o/");
            self.is_outdated.store(false, atomic::Ordering::SeqCst);
            true
        } else {
            store
                .set_kv_data(
                    StateStoreDataKey::ThreadSubscriptionsCatchupTokens,
                    StateStoreDataValue::ThreadSubscriptionsCatchupTokens(tokens),
                )
                .await?;

            trace!("Marking thread subscriptions as outdated.");
            self.is_outdated.store(true, atomic::Ordering::SeqCst);
            false
        };
        Ok(is_empty)
    }
}

pub struct ThreadSubscriptionCatchup {
    /// The task catching up thread subscriptions in the background.
    _task: OnceCell<AbortOnDrop<()>>,

    /// Whether the known list of thread subscriptions is outdated or not, i.e.
    /// all thread subscriptions have been caught up
    is_outdated: Arc<AtomicBool>,

    /// A weak reference to the parent [`Client`] instance.
    client: WeakClient,

    /// A sender to wake up the catchup task when new catchup tokens are
    /// available.
    ping_sender: Sender<()>,

    /// A mutex to ensure there's only one writer on the thread subscriptions
    /// catchup tokens at a time.
    uniq_mutex: Arc<Mutex<()>>,
}

impl ThreadSubscriptionCatchup {
    pub fn new(client: Client) -> Arc<Self> {
        let is_outdated = Arc::new(AtomicBool::new(true));

        let weak_client = WeakClient::from_client(&client);

        let (ping_sender, ping_receiver) = channel(8);

        let uniq_mutex = Arc::new(Mutex::new(()));

        let this = Arc::new(Self {
            _task: OnceCell::new(),
            is_outdated,
            client: weak_client,
            ping_sender,
            uniq_mutex,
        });

        // Create the task only if the client is configured to handle thread
        // subscriptions.
        if client.enabled_thread_subscriptions() {
            let _ = this._task.get_or_init(|| {
                AbortOnDrop::new(spawn(Self::thread_subscriptions_catchup_task(
                    this.clone(),
                    ping_receiver,
                )))
            });
        }

        this
    }

    /// Returns whether the known list of thread subscriptions is outdated or
    /// more thread subscriptions need to be caught up.
    pub(crate) fn is_outdated(&self) -> bool {
        self.is_outdated.load(atomic::Ordering::SeqCst)
    }

    /// Store the new subscriptions changes, received via the sync response or
    /// from the msc4308 companion endpoint.
    #[instrument(skip_all)]
    pub(crate) async fn sync_subscriptions(
        &self,
        subscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadSubscription>>,
        unsubscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadUnsubscription>>,
        token: Option<ThreadSubscriptionCatchupToken>,
    ) -> Result<()> {
        // Precompute the updates so we don't hold the guard for too long.
        let updates = build_subscription_updates(&subscribed, &unsubscribed);
        let Some(guard) = self.lock().await else {
            // Client is shutting down.
            return Ok(());
        };
        self.save_catchup_token(&guard, token).await?;
        if !updates.is_empty() {
            trace!(
                "saving {} new subscriptions and {} unsubscriptions",
                subscribed.values().map(|by_room| by_room.len()).sum::<usize>(),
                unsubscribed.values().map(|by_room| by_room.len()).sum::<usize>(),
            );
            guard.client.state_store().upsert_thread_subscriptions(updates).await?;
        }
        Ok(())
    }

    /// Internal helper to lock writes to the thread subscriptions catchup
    /// tokens list.
    async fn lock(&self) -> Option<GuardedStoreAccess> {
        let client = self.client.get()?;
        let mutex_guard = self.uniq_mutex.clone().lock_owned().await;
        Some(GuardedStoreAccess {
            _mutex: mutex_guard,
            client,
            is_outdated: self.is_outdated.clone(),
        })
    }

    /// Save a new catchup token (or absence thereof) in the state store.
    async fn save_catchup_token(
        &self,
        guard: &GuardedStoreAccess,
        token: Option<ThreadSubscriptionCatchupToken>,
    ) -> Result<()> {
        // Note: saving an empty tokens list will mark the thread subscriptions list as
        // not outdated.
        let mut tokens = guard.load_catchup_tokens().await?.unwrap_or_default();

        if let Some(token) = token {
            trace!(?token, "Saving catchup token");
            tokens.push(token);
        } else {
            trace!("No catchup token to save");
        }

        let is_token_list_empty = guard.save_catchup_tokens(tokens).await?;

        // Wake up the catchup task, in case it's waiting.
        if !is_token_list_empty {
            let _ = self.ping_sender.send(()).await;
        }

        Ok(())
    }

    /// The background task listening to new catchup tokens, and using them to
    /// catch up the thread subscriptions via the [MSC4308] companion
    /// endpoint.
    ///
    /// It will continue to process catchup tokens until there are none, and
    /// then wait for a new one to be available and inserted in the
    /// database.
    ///
    /// It always processes catch up tokens from the newest to the oldest, since
    /// newest tokens are more interesting than older ones. Indeed, they're
    /// more likely to include entries with higher bump-stamps, i.e. to include
    /// more recent thread subscriptions statuses for each thread, so more
    /// relevant information.
    ///
    /// [MSC4308]: https://github.com/matrix-org/matrix-spec-proposals/pull/4308
    #[instrument(skip_all)]
    async fn thread_subscriptions_catchup_task(this: Arc<Self>, mut ping_receiver: Receiver<()>) {
        loop {
            // Load the current catchup token.
            let Some(guard) = this.lock().await else {
                // Client is shutting down.
                return;
            };

            let store_tokens = match guard.load_catchup_tokens().await {
                Ok(tokens) => tokens,
                Err(err) => {
                    warn!("Failed to load thread subscriptions catchup tokens: {err}");
                    continue;
                }
            };

            let Some(mut tokens) = store_tokens else {
                // Release the mutex.
                drop(guard);

                // Wait for a wake up.
                trace!("Waiting for an explicit wake up to process future thread subscriptions");

                if let Some(()) = ping_receiver.recv().await {
                    trace!("Woke up!");
                    continue;
                }

                // Channel closed, the client is shutting down.
                break;
            };

            // We do have a tokens. Pop the last value, and use it to catch up!
            let last = tokens.pop().expect("must be set per `load_catchup_tokens` contract");

            // Release the mutex before running the network request.
            let client = guard.client.clone();
            drop(guard);

            // Start the actual catchup!
            let req = assign!(ruma::api::client::threads::get_thread_subscriptions_changes::unstable::Request::new(), {
                from: Some(last.from.clone()),
                to: last.to.clone(),
            });

            match client.send(req).await {
                Ok(resp) => {
                    // Precompute the updates so we don't hold the guard for too long.
                    let updates = build_subscription_updates(&resp.subscribed, &resp.unsubscribed);

                    let guard = this
                        .lock()
                        .await
                        .expect("a client instance is alive, so the locking should not fail");

                    if !updates.is_empty() {
                        trace!(
                            "saving {} new subscriptions and {} unsubscriptions",
                            resp.subscribed.values().map(|by_room| by_room.len()).sum::<usize>(),
                            resp.unsubscribed.values().map(|by_room| by_room.len()).sum::<usize>(),
                        );

                        if let Err(err) =
                            guard.client.state_store().upsert_thread_subscriptions(updates).await
                        {
                            warn!("Failed to store caught up thread subscriptions: {err}");
                            continue;
                        }
                    }

                    // Refresh the tokens, as the list might have changed while we sent the
                    // request.
                    let mut tokens = match guard.load_catchup_tokens().await {
                        Ok(tokens) => tokens.unwrap_or_default(),
                        Err(err) => {
                            warn!("Failed to load thread subscriptions catchup tokens: {err}");
                            continue;
                        }
                    };

                    let Some(index) = tokens.iter().position(|t| *t == last) else {
                        warn!("Thread subscriptions catchup token disappeared while processing it");
                        continue;
                    };

                    if let Some(next_batch) = resp.end {
                        // If the response contained a next batch token, reuse the same catchup
                        // token entry, so the `to` value remains the same.
                        tokens[index] =
                            ThreadSubscriptionCatchupToken { from: next_batch, to: last.to };
                    } else {
                        // No next batch, we can remove this token from the list.
                        tokens.remove(index);
                    }

                    if let Err(err) = guard.save_catchup_tokens(tokens).await {
                        warn!("Failed to save updated thread subscriptions catchup tokens: {err}");
                    }
                }

                Err(err) => {
                    warn!("Failed to catch up thread subscriptions: {err}");
                }
            }
        }
    }
}

/// Internal helper for building the thread subscription updates Vec.
fn build_subscription_updates<'a>(
    subscribed: &'a BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadSubscription>>,
    unsubscribed: &'a BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadUnsubscription>>,
) -> Vec<(&'a RoomId, &'a EventId, StoredThreadSubscription)> {
    let mut updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)> =
        Vec::with_capacity(unsubscribed.len() + subscribed.len());

    // Take into account the new unsubscriptions.
    for (room_id, room_map) in unsubscribed {
        for (event_id, thread_sub) in room_map {
            updates.push((
                room_id,
                event_id,
                StoredThreadSubscription {
                    status: ThreadSubscriptionStatus::Unsubscribed,
                    bump_stamp: Some(thread_sub.bump_stamp.into()),
                },
            ));
        }
    }

    // Take into account the new subscriptions.
    for (room_id, room_map) in subscribed {
        for (event_id, thread_sub) in room_map {
            updates.push((
                room_id,
                event_id,
                StoredThreadSubscription {
                    status: ThreadSubscriptionStatus::Subscribed {
                        automatic: thread_sub.automatic,
                    },
                    bump_stamp: Some(thread_sub.bump_stamp.into()),
                },
            ));
        }
    }

    updates
}

#[cfg(test)]
mod tests {
    use std::ops::Not as _;

    use matrix_sdk_base::ThreadSubscriptionCatchupToken;
    use matrix_sdk_test::async_test;

    use crate::test_utils::client::MockClientBuilder;

    #[async_test]
    async fn test_load_save_catchup_tokens() {
        let client = MockClientBuilder::new(None).build().await;

        let tsc = client.thread_subscription_catchup();

        // At first there are no catchup tokens, and we are outdated.
        let guard = tsc.lock().await.unwrap();
        assert!(guard.load_catchup_tokens().await.unwrap().is_none());
        assert!(tsc.is_outdated());

        // When I save a token,
        let token =
            ThreadSubscriptionCatchupToken { from: "from".to_owned(), to: Some("to".to_owned()) };
        guard.save_catchup_tokens(vec![token.clone()]).await.unwrap();

        // Well, it is saved,
        let tokens = guard.load_catchup_tokens().await.unwrap();
        assert_eq!(tokens, Some(vec![token]));

        // And we are still outdated.
        assert!(tsc.is_outdated());

        // When I remove the token,
        guard.save_catchup_tokens(vec![]).await.unwrap();

        // It is gone,
        assert!(guard.load_catchup_tokens().await.unwrap().is_none());

        // And we are not outdated anymore!
        assert!(tsc.is_outdated().not());
    }
}
