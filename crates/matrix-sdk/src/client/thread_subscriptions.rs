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
        atomic::{self, AtomicBool},
        Arc,
    },
};

use matrix_sdk_base::{
    executor::AbortOnDrop,
    store::{StoredThreadSubscription, ThreadSubscriptionStatus},
    StateStoreDataKey, StateStoreDataValue, ThreadSubscriptionCatchupToken,
};
use once_cell::sync::OnceCell;
use ruma::{
    api::client::threads::get_thread_subscriptions_changes::unstable::{
        ThreadSubscription, ThreadUnsubscription,
    },
    assign, OwnedEventId, OwnedRoomId,
};
use tokio::{
    spawn,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex, OwnedMutexGuard,
    },
};
use tracing::{error, instrument, trace, warn};

use crate::{client::WeakClient, Client, Result};

struct GuardedStoreAccess {
    _mutex: OwnedMutexGuard<()>,
    client: Client,
    is_outdated: Arc<AtomicBool>,
}

impl GuardedStoreAccess {
    /// Return the current list of catchup tokens, if any.
    ///
    /// It is guaranteed that if the list is set, then it's non-empty.
    async fn load_catchup_tokens(&self) -> Option<Vec<ThreadSubscriptionCatchupToken>> {
        match self
            .client
            .state_store()
            .get_kv_data(StateStoreDataKey::ThreadSubscriptionsCatchupTokens)
            .await
        {
            Ok(Some(data)) => {
                if let Some(tokens) = data.into_thread_subscriptions_catchup_tokens() {
                    // If the tokens list is empty, automatically clean it up.
                    if tokens.is_empty() {
                        self.save_catchup_tokens(tokens).await;
                    } else {
                        return Some(tokens);
                    }
                } else {
                    warn!(
                        "invalid data in thread subscriptions catchup tokens state store k/v entry"
                    );
                }
            }

            Ok(None) => {}

            Err(err) => {
                error!("Failed to load thread subscriptions catchup tokens: {err}");
            }
        }

        None
    }

    #[instrument(skip_all, fields(num_tokens = tokens.len()))]
    async fn save_catchup_tokens(&self, tokens: Vec<ThreadSubscriptionCatchupToken>) {
        let store = self.client.state_store();
        if tokens.is_empty() {
            if let Err(err) =
                store.remove_kv_data(StateStoreDataKey::ThreadSubscriptionsCatchupTokens).await
            {
                error!("Failed to remove thread subscriptions catchup tokens: {err}");
            } else {
                trace!("Marking thread subscriptions as not outdated \\o/");
                self.is_outdated.store(false, atomic::Ordering::SeqCst);
            }
        } else if let Err(err) = store
            .set_kv_data(
                StateStoreDataKey::ThreadSubscriptionsCatchupTokens,
                StateStoreDataValue::ThreadSubscriptionsCatchupTokens(tokens),
            )
            .await
        {
            error!("Failed to set thread subscriptions catchup tokens: {err}");
        } else {
            trace!("Marking thread subscriptions as outdated.");
            self.is_outdated.store(true, atomic::Ordering::SeqCst);
        }
    }
}

pub struct ThreadSubscriptionCatchup {
    _task: OnceCell<AbortOnDrop<()>>,

    is_outdated: Arc<AtomicBool>,

    client: WeakClient,

    ping_sender: Sender<()>,

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

        if client.enabled_thread_subscriptions() {
            let _ = this._task.get_or_init(|| {
                AbortOnDrop::new(spawn(Self::catchup_task(this.clone(), ping_receiver)))
            });
        }

        this
    }

    pub(crate) fn is_outdated(&self) -> bool {
        self.is_outdated.load(atomic::Ordering::SeqCst)
    }

    #[instrument(skip_all)]
    pub(crate) async fn store_subscriptions(
        &self,
        subscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadSubscription>>,
        unsubscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadUnsubscription>>,
    ) -> Result<()> {
        let Some(client) = self.client.get() else {
            // Client is shutting down.
            return Ok(());
        };

        if subscribed.is_empty() && unsubscribed.is_empty() {
            // Nothing to do.
            return Ok(());
        }

        trace!(
            "saving {} new subscriptions and {} unsubscriptions",
            subscribed.len(),
            unsubscribed.len()
        );

        // Take into account the new unsubscriptions.
        for (room_id, room_map) in unsubscribed {
            for (event_id, thread_sub) in room_map {
                client
                    .state_store()
                    .upsert_thread_subscription(
                        &room_id,
                        &event_id,
                        StoredThreadSubscription {
                            status: ThreadSubscriptionStatus::Unsubscribed,
                            bump_stamp: Some(thread_sub.bump_stamp.into()),
                        },
                    )
                    .await?;
            }
        }

        // Take into account the new subscriptions.
        for (room_id, room_map) in subscribed {
            for (event_id, thread_sub) in room_map {
                client
                    .state_store()
                    .upsert_thread_subscription(
                        &room_id,
                        &event_id,
                        StoredThreadSubscription {
                            status: ThreadSubscriptionStatus::Subscribed {
                                automatic: thread_sub.automatic,
                            },
                            bump_stamp: Some(thread_sub.bump_stamp.into()),
                        },
                    )
                    .await?;
            }
        }

        Ok(())
    }

    async fn lock(&self) -> Option<GuardedStoreAccess> {
        let client = self.client.get()?;
        let mutex_guard = self.uniq_mutex.clone().lock_owned().await;
        Some(GuardedStoreAccess {
            _mutex: mutex_guard,
            client,
            is_outdated: self.is_outdated.clone(),
        })
    }

    #[instrument(skip_all)]
    pub async fn save_catchup_token(
        &self,
        token: Option<ThreadSubscriptionCatchupToken>,
    ) -> Result<()> {
        let Some(guard) = self.lock().await else {
            // Client is shutting down.
            return Ok(());
        };

        // Note: saving an empty tokens list will mark the thread subscriptions list as
        // not outdated.
        let mut tokens = guard.load_catchup_tokens().await.unwrap_or_default();

        if let Some(token) = token {
            trace!(?token, "Saving catchup token");
            tokens.push(token);
        } else {
            trace!("No catchup token to save");
        }

        let has_tokens = !tokens.is_empty();

        guard.save_catchup_tokens(tokens).await;

        // Wake up the catchup task, if it's waiting.
        if has_tokens {
            let _ = self.ping_sender.send(()).await;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn catchup_task(this: Arc<Self>, mut ping_receiver: Receiver<()>) {
        loop {
            // Load the current catchup token.
            let Some(guard) = this.lock().await else {
                // Client is shutting down.
                return;
            };

            let Some(mut tokens) = guard.load_catchup_tokens().await else {
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
                    let guard = this
                        .lock()
                        .await
                        .expect("a client instance is alive, so the locking should not fail");

                    if let Err(err) =
                        this.store_subscriptions(resp.subscribed, resp.unsubscribed).await
                    {
                        error!("Failed to store caught up thread subscriptions: {err}");
                        continue;
                    }

                    // Refresh the tokens, as the list might have changed while we sent the
                    // request.
                    let mut tokens = guard.load_catchup_tokens().await.unwrap_or_default();

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

                    guard.save_catchup_tokens(tokens).await;
                }

                Err(err) => {
                    error!("Failed to catch up thread subscriptions: {err}");
                }
            }
        }
    }
}
