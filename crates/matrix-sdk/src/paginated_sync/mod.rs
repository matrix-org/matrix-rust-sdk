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
// See the License for that specific language governing permissions and
// limitations under the License.

//! Paginated Sync (MSC TBD): a dialect of Simplified Sliding Sync (MSC4186)
//! without lists, ranges, subscriptions or expanding timelines.
//!
//! The client tells the server the biggest response it can handle - at most
//! `page_size` rooms per response, at most `limit` new events per room (with
//! an explicit per-room gap beyond that), `history` events for rooms the
//! connection has never seen - and the server pages it through whatever has
//! changed, most recently active rooms first. The response reports `pending`,
//! the number of changed rooms that didn't fit; while it is non-zero the
//! server answers immediately and the client keeps syncing to drain the
//! backlog.
//!
//! There is no request-shaping state to get wrong (no ranges to grow, no
//! sticky lists, no subscriptions) and the response processing is exactly
//! MSC4186's, reused verbatim.

mod http;

use std::sync::Arc;
use std::time::Duration;

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::RequestedRequiredStates;
use matrix_sdk_common::executor::spawn;
use tokio::{
    select,
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast::Sender},
};
use tracing::{Instrument, Span, debug, error, info, instrument, warn};

use async_stream::stream;
use futures_core::stream::Stream;
use ruma::{
    UserId,
    api::client::sync::sync_events::v5,
    assign,
    events::StateEventType,
};

pub use self::http::{Request, Response};
use crate::sliding_sync::{SlidingSyncResponseProcessor, UpdateSummary};
use crate::{Client, Result, config::RequestConfig};

/// The loading state of a [`PaginatedSync`] connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaginatedSyncLoadingState {
    /// No response has been received yet on this connection.
    NotLoaded,

    /// Responses have been received, but the server reports a backlog of rooms
    /// still to be delivered.
    PartiallyLoaded {
        /// How many rooms the server still has queued for us.
        pending: u32,
    },

    /// The server has no backlog: everything it wanted to send has been sent.
    FullyLoaded,
}

/// Internal message to control the sync loop.
#[derive(Copy, Clone, Debug, PartialEq)]
enum InternalMessage {
    SyncLoopStop,
}

/// A Paginated Sync instance. Cheap to clone.
#[derive(Clone, Debug)]
pub struct PaginatedSync {
    inner: Arc<PaginatedSyncInner>,
}

#[derive(Debug)]
struct PaginatedSyncInner {
    /// A unique identifier for this connection (the `conn_id`).
    id: String,

    /// The HTTP Matrix client.
    client: Client,

    /// `page_size` for the first request of a session (kept small so the
    /// first response - the top of the room list - renders fast).
    initial_page_size: u32,

    /// `page_size` for every subsequent request.
    page_size: u32,

    /// Maximum number of new timeline events per room per response.
    limit: u32,

    /// Number of timeline events for rooms never sent on this connection.
    history: u32,

    /// The state returned for every room.
    required_state: Vec<(StateEventType, String)>,

    /// Extensions (MSC4186's, verbatim).
    extensions: v5::request::Extensions,

    /// Long-polling timeout.
    poll_timeout: Duration,

    /// Extra network time on top of [`Self::poll_timeout`] for the request
    /// timeout.
    network_timeout: Duration,

    /// The `pos` marker. The mutex is held from request generation until the
    /// response has been fully handled, serialising rounds.
    position: Arc<AsyncMutex<Option<String>>>,

    /// The connection's loading state (whether the server has a backlog).
    loading_state: SharedObservable<PaginatedSyncLoadingState>,

    /// The total number of rooms in the account, from the last response.
    total_rooms: SharedObservable<Option<u32>>,

    /// Internal channel to control the sync loop.
    internal_channel: Sender<InternalMessage>,
}

impl PaginatedSync {
    /// Create a [`PaginatedSyncBuilder`] for this client.
    pub fn builder(id: impl Into<String>, client: Client) -> PaginatedSyncBuilder {
        PaginatedSyncBuilder::new(id.into(), client)
    }

    fn storage_key(&self) -> String {
        format_storage_key(&self.inner.id, self.inner.client.user_id())
    }

    /// Subscribe to the connection's loading state.
    pub fn loading_state(&self) -> Subscriber<PaginatedSyncLoadingState> {
        self.inner.loading_state.subscribe_reset()
    }

    /// Get the current loading state.
    pub fn current_loading_state(&self) -> PaginatedSyncLoadingState {
        self.inner.loading_state.get()
    }

    /// Subscribe to the total number of rooms in the account.
    pub fn total_rooms(&self) -> Subscriber<Option<u32>> {
        self.inner.total_rooms.subscribe_reset()
    }

    /// Get the current total number of rooms, if a response carried it.
    pub fn current_total_rooms(&self) -> Option<u32> {
        self.inner.total_rooms.get()
    }

    /// Send a single request and handle its response.
    #[instrument(skip_all, fields(conn_id = self.inner.id, pos))]
    pub async fn sync_once(&self) -> Result<UpdateSummary> {
        let mut position_guard = self.inner.position.clone().lock_owned().await;

        let pos = position_guard.clone();
        Span::current().record("pos", &pos);

        // MSC4186 semantics (inherited here): a request without a `pos` only
        // returns device-list changes since `pos`, i.e. none - so the device
        // list cache must be re-downloaded from scratch.
        #[cfg(feature = "e2e-encryption")]
        if pos.is_none() && self.inner.extensions.e2ee.enabled == Some(true) {
            info!("Marking all tracked users as dirty");

            let olm_machine = self.inner.client.olm_machine().await;
            let olm_machine =
                olm_machine.as_ref().ok_or(crate::sliding_sync::Error::NoOlmMachine)?;
            olm_machine.mark_all_tracked_users_as_dirty().await?;
        }

        let page_size = if pos.is_none() {
            self.inner.initial_page_size
        } else {
            self.inner.page_size
        };

        let request = assign!(Request::new(), {
            conn_id: Some(self.inner.id.clone()),
            pos,
            // The server answers immediately whenever it has data or a
            // backlog; the timeout only applies when we are fully caught up.
            timeout: Some(self.inner.poll_timeout),
            page_size: page_size.into(),
            limit: self.inner.limit.into(),
            history: Some(self.inner.history.into()),
            required_state: self.inner.required_state.clone(),
            extensions: self.inner.extensions.clone(),
        });

        let requested_required_states =
            RequestedRequiredStates::new(self.inner.required_state.clone(), Default::default());

        let request_config = RequestConfig::default()
            .timeout(self.inner.poll_timeout + self.inner.network_timeout)
            .retry_limit(3);

        debug!("Sending request");

        let response =
            self.inner.client.send(request).with_request_config(request_config).await?;

        debug!("Received response");

        // Handle the response in a spawned (thus uncancellable) future, so a
        // dropped caller cannot leave the client in a half-processed state.
        // The `position` guard is held throughout and released at the end.
        let this = self.clone();

        let future = async move {
            let summary = this
                .handle_response(response, &mut position_guard, requested_required_states)
                .await?;

            this.persist_pos(&position_guard).await;

            drop(position_guard);

            Ok::<_, crate::Error>(summary)
        };

        let summary = spawn(future.instrument(Span::current())).await.map_err(|error| {
            crate::sliding_sync::Error::JoinError {
                task_description: "paginated_sync_handle_response".to_owned(),
                error,
            }
        })??;

        // Notify a new sync was received.
        self.inner.client.inner.sync_beat.notify(usize::MAX);

        Ok(summary)
    }

    /// Handle a response: feed it through the (shared, MSC4186) response
    /// processing pipeline, then update our own bookkeeping.
    #[instrument(skip_all)]
    async fn handle_response(
        &self,
        response: Response,
        position: &mut OwnedMutexGuard<Option<String>>,
        requested_required_states: RequestedRequiredStates,
    ) -> Result<UpdateSummary> {
        let pending: u32 = response.pending.try_into().unwrap_or(u32::MAX);
        let total_rooms: Option<u32> =
            response.total_rooms.map(|total| total.try_into().unwrap_or(u32::MAX));

        // From here on the response is MSC4186-shaped; the whole processing
        // pipeline is reused.
        let response: v5::Response = response.into();

        let new_pos = response.pos.clone();

        // Register the response's rooms with the latest-events machinery
        // BEFORE processing, and outside the state store lock (see the
        // sliding sync equivalent for the reasoning).
        crate::sync::subscribe_to_room_latest_events(&self.inner.client, response.rooms.keys())
            .await;

        let sync_response = {
            let response_processor = {
                let state_store_guard =
                    self.inner.client.base_client().state_store_lock().lock().await;

                let mut response_processor =
                    SlidingSyncResponseProcessor::new(self.inner.client.clone());

                #[cfg(feature = "e2e-encryption")]
                if self.inner.extensions.e2ee.enabled == Some(true) {
                    response_processor
                        .handle_encryption(&response.extensions, &state_store_guard)
                        .await?;
                }

                response_processor
                    .handle_room_response(
                        &response,
                        &requested_required_states,
                        &state_store_guard,
                    )
                    .await?;

                response_processor
            };

            // The state store lock is released; run the event handlers.
            response_processor.process_and_take_response().await?
        };

        // Newly-known rooms can compute their latest event now.
        crate::sync::compute_missing_room_latest_events(
            &self.inner.client,
            response.rooms.keys(),
        )
        .await;

        // Update the connection's observables.
        if total_rooms.is_some() {
            self.inner.total_rooms.set_if_not_eq(total_rooms);
        }
        self.inner.loading_state.set_if_not_eq(if pending == 0 {
            PaginatedSyncLoadingState::FullyLoaded
        } else {
            PaginatedSyncLoadingState::PartiallyLoaded { pending }
        });

        let update_summary = {
            let mut updated_rooms = Vec::with_capacity(
                response.rooms.len() + sync_response.rooms.joined.len(),
            );
            updated_rooms.extend(response.rooms.keys().cloned());
            // Rooms only mentioned by extensions.
            updated_rooms.extend(sync_response.rooms.joined.keys().cloned());

            UpdateSummary { lists: Vec::new(), rooms: updated_rooms }
        };

        debug!(previous_pos = ?position, new_pos, pending, "Updating `pos`");

        **position = Some(new_pos);

        Ok(update_summary)
    }

    /// Create the sync loop.
    #[instrument(name = "paginated_sync_stream", skip_all, fields(conn_id = self.inner.id))]
    pub fn sync(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        debug!("Starting sync stream");

        let mut internal_channel_receiver = self.inner.internal_channel.subscribe();

        stream! {
            loop {
                select! {
                    biased;

                    internal_message = internal_channel_receiver.recv() => {
                        debug!(?internal_message, "Sync stream has received an internal message");

                        match internal_message {
                            Err(_) | Ok(InternalMessage::SyncLoopStop) => break,
                        }
                    }

                    update_summary = self.sync_once() => {
                        match update_summary {
                            Ok(updates) => yield Ok(updates),

                            // There is no protocol error path: a server that
                            // doesn't recognise our `pos` starts the connection
                            // afresh and re-sends rooms as never-sent, all by
                            // itself. Anything landing here is a genuine
                            // transport/auth failure.
                            Err(error) => {
                                yield Err(error);

                                break;
                            }
                        }
                    }
                }
            }

            debug!("Sync stream has exited.");
        }
    }

    /// Stop the sync loop, if running.
    pub fn stop_sync(&self) -> Result<()> {
        let _ = self.inner.internal_channel.send(InternalMessage::SyncLoopStop);
        Ok(())
    }

    /// Expire the current session: reset `pos` (in memory and on disk).
    ///
    /// Unlike sliding sync there are no lists or ranges to rebuild: after
    /// expiry the server re-sends rooms as never-sent (`history` events each)
    /// as the client pages through them again.
    pub async fn expire_session(&self) {
        info!("Session expired; resetting `pos`");

        let mut position = self.inner.position.lock().await;
        *position = None;

        self.inner.loading_state.set_if_not_eq(PaginatedSyncLoadingState::NotLoaded);

        let storage_key = self.storage_key();
        let _ = self
            .inner
            .client
            .state_store()
            .remove_custom_value(storage_key.as_bytes())
            .await;
    }

    /// Persist `pos` to the state store, so an app restart resumes the
    /// connection instead of starting a fresh one.
    async fn persist_pos(&self, position: &OwnedMutexGuard<Option<String>>) {
        let storage_key = self.storage_key();

        let result = match position.as_deref() {
            Some(pos) => {
                self.inner
                    .client
                    .state_store()
                    .set_custom_value(storage_key.as_bytes(), pos.as_bytes().to_vec())
                    .await
            }
            None => {
                self.inner.client.state_store().remove_custom_value(storage_key.as_bytes()).await
            }
        };

        if let Err(error) = result {
            warn!(?error, "Failed to persist the paginated sync `pos`");
        }
    }
}

fn format_storage_key(id: &str, user_id: Option<&UserId>) -> String {
    let user_id = user_id.map(|user_id| user_id.as_str()).unwrap_or("");
    format!("paginated_sync::{id}::{user_id}::pos")
}

/// Builder for [`PaginatedSync`].
#[derive(Clone, Debug)]
pub struct PaginatedSyncBuilder {
    id: String,
    client: Client,
    initial_page_size: u32,
    page_size: u32,
    limit: u32,
    history: u32,
    required_state: Vec<(StateEventType, String)>,
    extensions: v5::request::Extensions,
    poll_timeout: Duration,
    network_timeout: Duration,
}

impl PaginatedSyncBuilder {
    fn new(id: String, client: Client) -> Self {
        Self {
            id,
            client,
            initial_page_size: 20,
            page_size: 100,
            limit: 10,
            history: 1,
            required_state: Vec::new(),
            extensions: Default::default(),
            poll_timeout: Duration::from_secs(30),
            network_timeout: Duration::from_secs(30),
        }
    }

    /// Set the `page_size` used for the first request of a session.
    pub fn initial_page_size(mut self, value: u32) -> Self {
        self.initial_page_size = value;
        self
    }

    /// Set the `page_size` used for every request but the first.
    pub fn page_size(mut self, value: u32) -> Self {
        self.page_size = value;
        self
    }

    /// Set `limit`, the maximum number of new timeline events per room per
    /// response.
    pub fn limit(mut self, value: u32) -> Self {
        self.limit = value;
        self
    }

    /// Set `history`, the number of timeline events for rooms never sent on
    /// the connection.
    pub fn history(mut self, value: u32) -> Self {
        self.history = value;
        self
    }

    /// Set the `required_state`, applied to every room.
    pub fn required_state(mut self, value: Vec<(StateEventType, String)>) -> Self {
        self.required_state = value;
        self
    }

    /// Set the extensions configuration (MSC4186's, verbatim).
    pub fn extensions(mut self, value: v5::request::Extensions) -> Self {
        self.extensions = value;
        self
    }

    /// Set the long-polling timeout.
    pub fn poll_timeout(mut self, value: Duration) -> Self {
        self.poll_timeout = value;
        self
    }

    /// Build the [`PaginatedSync`], restoring a persisted `pos` if there is
    /// one.
    pub async fn build(self) -> Result<PaginatedSync> {
        let (internal_channel, _) = tokio::sync::broadcast::channel(8);

        let restored_pos = {
            let storage_key = format_storage_key(&self.id, self.client.user_id());

            match self.client.state_store().get_custom_value(storage_key.as_bytes()).await {
                Ok(Some(bytes)) => match String::from_utf8(bytes) {
                    Ok(pos) => {
                        debug!(pos, "Restored the paginated sync `pos`");
                        Some(pos)
                    }
                    Err(error) => {
                        error!(?error, "Invalid persisted paginated sync `pos`; ignoring");
                        None
                    }
                },
                Ok(None) => None,
                Err(error) => {
                    warn!(?error, "Failed to restore the paginated sync `pos`");
                    None
                }
            }
        };

        Ok(PaginatedSync {
            inner: Arc::new(PaginatedSyncInner {
                id: self.id,
                client: self.client,
                initial_page_size: self.initial_page_size,
                page_size: self.page_size,
                limit: self.limit,
                history: self.history,
                required_state: self.required_state,
                extensions: self.extensions,
                poll_timeout: self.poll_timeout,
                network_timeout: self.network_timeout,
                position: Arc::new(AsyncMutex::new(restored_pos)),
                loading_state: SharedObservable::new(PaginatedSyncLoadingState::NotLoaded),
                total_rooms: SharedObservable::new(None),
                internal_channel,
            }),
        })
    }
}
