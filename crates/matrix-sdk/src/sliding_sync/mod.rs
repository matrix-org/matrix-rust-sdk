// Copyright 2022-2023 Benjamin Kampmann
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
// See the License for that specific language governing permissions and
// limitations under the License.

#![doc = include_str!("README.md")]

mod builder;
mod cache;
mod client;
mod error;
mod list;
mod sticky_parameters;
mod utils;

use std::{
    collections::{btree_map::Entry, BTreeMap},
    fmt::Debug,
    future::Future,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use async_stream::stream;
pub use client::{Version, VersionBuilder};
use futures_core::stream::Stream;
use matrix_sdk_base::RequestedRequiredStates;
use matrix_sdk_common::{executor::spawn, timer};
use ruma::{
    api::client::{error::ErrorKind, sync::sync_events::v5 as http},
    assign, OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::{broadcast::Sender, Mutex as AsyncMutex, OwnedMutexGuard, RwLock as AsyncRwLock},
};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};

#[cfg(feature = "e2e-encryption")]
use self::utils::JoinHandleExt as _;
pub use self::{builder::*, client::VersionBuilderError, error::*, list::*};
use self::{
    cache::restore_sliding_sync_state,
    client::SlidingSyncResponseProcessor,
    sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager, StickyData},
};
use crate::{config::RequestConfig, Client, Result};

/// The Sliding Sync instance.
///
/// It is OK to clone this type as much as you need: cloning it is cheap.
#[derive(Clone, Debug)]
pub struct SlidingSync {
    /// The Sliding Sync data.
    inner: Arc<SlidingSyncInner>,
}

#[derive(Debug)]
pub(super) struct SlidingSyncInner {
    /// A unique identifier for this instance of sliding sync.
    ///
    /// Used to distinguish different connections to sliding sync.
    id: String,

    /// The HTTP Matrix client.
    client: Client,

    /// Long-polling timeout that appears in sliding sync request.
    poll_timeout: Duration,

    /// Extra duration for the sliding sync request to timeout. This is added to
    /// the [`Self::proxy_timeout`].
    network_timeout: Duration,

    /// The storage key to keep this cache at and load it from.
    storage_key: String,

    /// Should this sliding sync instance try to restore its sync position
    /// from the database?
    ///
    /// Note: in non-cfg(e2e-encryption) builds, it's always set to false. We
    /// keep it even so, to avoid sparkling cfg statements everywhere
    /// throughout this file.
    share_pos: bool,

    /// Position markers.
    ///
    /// The `pos` marker represents a progression when exchanging requests and
    /// responses with the server: the server acknowledges the request by
    /// responding with a new `pos`. If the client sends two non-necessarily
    /// consecutive requests with the same `pos`, the server has to reply with
    /// the same identical response.
    ///
    /// `position` is behind a mutex so that a new request starts after the
    /// previous request trip has fully ended (successfully or not). This
    /// mechanism exists to wait for the response to be handled and to see the
    /// `position` being updated, before sending a new request.
    position: Arc<AsyncMutex<SlidingSyncPositionMarkers>>,

    /// The lists of this Sliding Sync instance.
    lists: AsyncRwLock<BTreeMap<String, SlidingSyncList>>,

    /// Request parameters that are sticky.
    sticky: StdRwLock<SlidingSyncStickyManager<SlidingSyncStickyParameters>>,

    /// Internal channel used to pass messages between Sliding Sync and other
    /// types.
    internal_channel: Sender<SlidingSyncInternalMessage>,
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner) }
    }

    async fn cache_to_storage(&self, position: &SlidingSyncPositionMarkers) -> Result<()> {
        cache::store_sliding_sync_state(self, position).await
    }

    /// Create a new [`SlidingSyncBuilder`].
    pub fn builder(id: String, client: Client) -> Result<SlidingSyncBuilder, Error> {
        SlidingSyncBuilder::new(id, client)
    }

    /// Subscribe to many rooms.
    ///
    /// If the associated `Room`s exist, it will be marked as
    /// members are missing, so that it ensures to re-fetch all members.
    ///
    /// A subscription to an already subscribed room is ignored.
    pub fn subscribe_to_rooms(
        &self,
        room_ids: &[&RoomId],
        settings: Option<http::request::RoomSubscription>,
        cancel_in_flight_request: bool,
    ) {
        let settings = settings.unwrap_or_default();
        let mut sticky = self.inner.sticky.write().unwrap();
        let room_subscriptions = &mut sticky.data_mut().room_subscriptions;

        let mut skip_over_current_sync_loop_iteration = false;

        for room_id in room_ids {
            // If the room subscription already exists, let's not
            // override it with a new one. First, it would reset its
            // state (`RoomSubscriptionState`), and second it would try to
            // re-subscribe with the next request. We don't want that. A room
            // subscription should happen once, and next subscriptions should
            // be ignored.
            if let Entry::Vacant(entry) = room_subscriptions.entry((*room_id).to_owned()) {
                if let Some(room) = self.inner.client.get_room(room_id) {
                    room.mark_members_missing();
                }

                entry.insert((RoomSubscriptionState::default(), settings.clone()));

                skip_over_current_sync_loop_iteration = true;
            }
        }

        if cancel_in_flight_request && skip_over_current_sync_loop_iteration {
            self.inner.internal_channel_send_if_possible(
                SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
            );
        }
    }

    /// Find a list by its name, and do something on it if it exists.
    pub async fn on_list<Function, FunctionOutput, R>(
        &self,
        list_name: &str,
        function: Function,
    ) -> Option<R>
    where
        Function: FnOnce(&SlidingSyncList) -> FunctionOutput,
        FunctionOutput: Future<Output = R>,
    {
        let lists = self.inner.lists.read().await;

        match lists.get(list_name) {
            Some(list) => Some(function(list).await),
            None => None,
        }
    }

    /// Add the list to the list of lists.
    ///
    /// As lists need to have a unique `.name`, if a list with the same name
    /// is found the new list will replace the old one and the return it or
    /// `None`.
    pub async fn add_list(
        &self,
        list_builder: SlidingSyncListBuilder,
    ) -> Result<Option<SlidingSyncList>> {
        let list = list_builder.build(self.inner.internal_channel.clone());

        let old_list = self.inner.lists.write().await.insert(list.name().to_owned(), list);

        self.inner.internal_channel_send_if_possible(
            SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
        );

        Ok(old_list)
    }

    /// Add a list that will be cached and reloaded from the cache.
    ///
    /// This will raise an error if a storage key was not set, or if there
    /// was a I/O error reading from the cache.
    ///
    /// The rest of the semantics is the same as [`Self::add_list`].
    pub async fn add_cached_list(
        &self,
        mut list_builder: SlidingSyncListBuilder,
    ) -> Result<Option<SlidingSyncList>> {
        let _timer = timer!(format!("restoring (loading+processing) list {}", list_builder.name));

        list_builder.set_cached_and_reload(&self.inner.client, &self.inner.storage_key).await?;

        self.add_list(list_builder).await
    }

    /// Handle the HTTP response.
    #[instrument(skip_all)]
    async fn handle_response(
        &self,
        sliding_sync_response: http::Response,
        position: &mut SlidingSyncPositionMarkers,
        requested_required_states: RequestedRequiredStates,
    ) -> Result<UpdateSummary, crate::Error> {
        let pos = Some(sliding_sync_response.pos.clone());

        let must_process_rooms_response = self.must_process_rooms_response().await;

        trace!(yes = must_process_rooms_response, "Must process rooms response?");

        // Transform a Sliding Sync Response to a `SyncResponse`.
        //
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `http::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.

        let sync_response = {
            let response_processor = {
                // Take the lock to avoid concurrent sliding syncs overwriting each other's room
                // infos.
                let _sync_lock = self.inner.client.base_client().sync_lock().lock().await;

                let mut response_processor =
                    SlidingSyncResponseProcessor::new(self.inner.client.clone());

                #[cfg(feature = "e2e-encryption")]
                if self.is_e2ee_enabled() {
                    response_processor.handle_encryption(&sliding_sync_response.extensions).await?
                }

                // Only handle the room's subsection of the response, if this sliding sync was
                // configured to do so.
                if must_process_rooms_response {
                    response_processor
                        .handle_room_response(&sliding_sync_response, &requested_required_states)
                        .await?;
                }

                response_processor
            };

            // Release the lock before calling event handlers
            response_processor.process_and_take_response().await?
        };

        debug!(?sync_response, "Sliding Sync response has been handled by the client");

        // Commit sticky parameters, if needed.
        if let Some(ref txn_id) = sliding_sync_response.txn_id {
            let txn_id = txn_id.as_str().into();
            self.inner.sticky.write().unwrap().maybe_commit(txn_id);
            let mut lists = self.inner.lists.write().await;
            lists.values_mut().for_each(|list| list.maybe_commit_sticky(txn_id));
        }

        let update_summary = {
            // Update the rooms.
            let updated_rooms = {
                let mut updated_rooms = Vec::with_capacity(
                    sliding_sync_response.rooms.len() + sync_response.rooms.joined.len(),
                );

                updated_rooms.extend(sliding_sync_response.rooms.keys().cloned());

                // There might be other rooms that were only mentioned in the sliding sync
                // extensions part of the response, and thus would result in rooms present in
                // the `sync_response.joined`. Mark them as updated too.
                //
                // Since we've removed rooms that were in the room subsection from
                // `sync_response.rooms.joined`, the remaining ones aren't already present in
                // `updated_rooms` and wouldn't cause any duplicates.
                updated_rooms.extend(sync_response.rooms.joined.keys().cloned());

                updated_rooms
            };

            // Update the lists.
            let updated_lists = {
                debug!(
                    lists = ?sliding_sync_response.lists,
                    "Update lists"
                );

                let mut updated_lists = Vec::with_capacity(sliding_sync_response.lists.len());
                let mut lists = self.inner.lists.write().await;

                // Iterate on known lists, not on lists in the response. Rooms may have been
                // updated that were not involved in any list update.
                for (name, list) in lists.iter_mut() {
                    if let Some(updates) = sliding_sync_response.lists.get(name) {
                        let maximum_number_of_rooms: u32 =
                            updates.count.try_into().expect("failed to convert `count` to `u32`");

                        if list.update(Some(maximum_number_of_rooms))? {
                            updated_lists.push(name.clone());
                        }
                    } else if list.update(None)? {
                        updated_lists.push(name.clone());
                    }
                }

                // Report about unknown lists.
                for name in sliding_sync_response.lists.keys() {
                    if !lists.contains_key(name) {
                        error!("Response for list `{name}` - unknown to us; skipping");
                    }
                }

                updated_lists
            };

            UpdateSummary { lists: updated_lists, rooms: updated_rooms }
        };

        // Everything went well, we can update the position markers.
        //
        // Save the new position markers.
        position.pos = pos;

        Ok(update_summary)
    }

    async fn generate_sync_request(
        &self,
        txn_id: &mut LazyTransactionId,
    ) -> Result<(http::Request, RequestConfig, OwnedMutexGuard<SlidingSyncPositionMarkers>)> {
        // Collect requests for lists.
        let mut requests_lists = BTreeMap::new();

        let require_timeout = {
            let lists = self.inner.lists.read().await;

            // Start at `true` in case there is zero list.
            let mut require_timeout = true;

            for (name, list) in lists.iter() {
                requests_lists.insert(name.clone(), list.next_request(txn_id)?);
                require_timeout = require_timeout && list.requires_timeout();
            }

            require_timeout
        };

        // Collect the `pos`.
        //
        // Wait on the `position` mutex to be available. It means no request nor
        // response is running. The `position` mutex is released whether the response
        // has been fully handled successfully, in this case the `pos` is updated, or
        // the response handling has failed, in this case the `pos` hasn't been updated
        // and the same `pos` will be used for this new request.
        let mut position_guard = self.inner.position.clone().lock_owned().await;

        let to_device_enabled =
            self.inner.sticky.read().unwrap().data().extensions.to_device.enabled == Some(true);

        let restored_fields = if self.inner.share_pos || to_device_enabled {
            restore_sliding_sync_state(&self.inner.client, &self.inner.storage_key).await?
        } else {
            None
        };

        // Update pos: either the one restored from the database, if any and the sliding
        // sync was configured so, or read it from the memory cache.
        let pos = if self.inner.share_pos {
            if let Some(fields) = &restored_fields {
                // Override the memory one with the database one, for consistency.
                if fields.pos != position_guard.pos {
                    info!(
                        "Pos from previous request ('{:?}') was different from \
                         pos in database ('{:?}').",
                        position_guard.pos, fields.pos
                    );
                    position_guard.pos = fields.pos.clone();
                }
                fields.pos.clone()
            } else {
                position_guard.pos.clone()
            }
        } else {
            position_guard.pos.clone()
        };

        Span::current().record("pos", &pos);

        // When the client sends a request with no `pos`, MSC4186 returns no device
        // lists updates, as it only returns changes since the provided `pos`
        // (which is `null` in this case); this is in line with sync v2.
        //
        // Therefore, with MSC4186, the device list cache must be marked as to be
        // re-downloaded if the `since` token is `None`, otherwise it's easy to miss
        // device lists updates that happened between the previous request and the new
        // “initial” request.
        #[cfg(feature = "e2e-encryption")]
        if pos.is_none() && self.is_e2ee_enabled() {
            info!("Marking all tracked users as dirty");

            let olm_machine = self.inner.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
            olm_machine.mark_all_tracked_users_as_dirty().await?;
        }

        // Configure the timeout.
        //
        // The `timeout` query is necessary when all lists require it. Please see
        // [`SlidingSyncList::requires_timeout`].
        let timeout = require_timeout.then(|| self.inner.poll_timeout);

        let mut request = assign!(http::Request::new(), {
            conn_id: Some(self.inner.id.clone()),
            pos,
            timeout,
            lists: requests_lists,
        });

        // Apply sticky parameters, if needs be.
        self.inner.sticky.write().unwrap().maybe_apply(&mut request, txn_id);

        // Extensions are now applied (via sticky parameters).
        //
        // Override the to-device token if the extension is enabled.
        if to_device_enabled {
            request.extensions.to_device.since =
                restored_fields.and_then(|fields| fields.to_device_token);
        }

        // Apply the transaction id if one was generated.
        if let Some(txn_id) = txn_id.get() {
            request.txn_id = Some(txn_id.to_string());
        }

        Ok((
            // The request itself.
            request,
            // Configure long-polling. We need some time for the long-poll itself,
            // and extra time for the network delays.
            RequestConfig::default()
                .timeout(self.inner.poll_timeout + self.inner.network_timeout)
                .retry_limit(3),
            position_guard,
        ))
    }

    /// Send a sliding sync request.
    ///
    /// This method contains the sending logic.
    async fn send_sync_request(
        &self,
        request: http::Request,
        request_config: RequestConfig,
        mut position_guard: OwnedMutexGuard<SlidingSyncPositionMarkers>,
    ) -> Result<UpdateSummary> {
        debug!("Sending request");

        // Prepare the request.
        let requested_required_states = RequestedRequiredStates::from(&request);
        let request = self.inner.client.send(request).with_request_config(request_config);

        // Send the request and get a response with end-to-end encryption support.
        //
        // Sending the `/sync` request out when end-to-end encryption is enabled means
        // that we need to also send out any outgoing e2ee related request out
        // coming from the `OlmMachine::outgoing_requests()` method.

        #[cfg(feature = "e2e-encryption")]
        let response = {
            if self.is_e2ee_enabled() {
                // Here, we need to run 2 things:
                //
                // 1. Send the sliding sync request and get a response,
                // 2. Send the E2EE requests.
                //
                // We don't want to use a `join` or `try_join` because we want to fail if and
                // only if sending the sliding sync request fails. Failing to send the E2EE
                // requests should just result in a log.
                //
                // We also want to give the priority to sliding sync request. E2EE requests are
                // sent concurrently to the sliding sync request, but the priority is on waiting
                // a sliding sync response.
                //
                // If sending sliding sync request fails, the sending of E2EE requests must be
                // aborted as soon as possible.

                let client = self.inner.client.clone();
                let e2ee_uploads = spawn(
                    async move {
                        if let Err(error) = client.send_outgoing_requests().await {
                            error!(?error, "Error while sending outgoing E2EE requests");
                        }
                    }
                    .instrument(Span::current()),
                )
                // Ensure that the task is not running in detached mode. It is aborted when it's
                // dropped.
                .abort_on_drop();

                // Wait on the sliding sync request success or failure early.
                let response = request.await?;

                // At this point, if `request` has been resolved successfully, we wait on
                // `e2ee_uploads`. It did run concurrently, so it should not be blocking for too
                // long. Otherwise —if `request` has failed— `e2ee_uploads` has
                // been dropped, so aborted.
                e2ee_uploads.await.map_err(|error| Error::JoinError {
                    task_description: "e2ee_uploads".to_owned(),
                    error,
                })?;

                response
            } else {
                request.await?
            }
        };

        // Send the request and get a response _without_ end-to-end encryption support.
        #[cfg(not(feature = "e2e-encryption"))]
        let response = request.await?;

        debug!("Received response");

        // At this point, the request has been sent, and a response has been received.
        //
        // We must ensure the handling of the response cannot be stopped/
        // cancelled. It must be done entirely, otherwise we can have
        // corrupted/incomplete states for Sliding Sync and other parts of
        // the code.
        //
        // That's why we are running the handling of the response in a spawned
        // future that cannot be cancelled by anything.
        let this = self.clone();

        // Spawn a new future to ensure that the code inside this future cannot be
        // cancelled if this method is cancelled.
        let future = async move {
            debug!("Start handling response");

            // In case the task running this future is detached, we must
            // ensure responses are handled one at a time. At this point we still own
            // `position_guard`, so we're fine.

            // Handle the response.
            let updates = this
                .handle_response(response, &mut position_guard, requested_required_states)
                .await?;

            this.cache_to_storage(&position_guard).await?;

            // Release the position guard lock.
            // It means that other responses can be generated and then handled later.
            drop(position_guard);

            debug!("Done handling response");

            Ok(updates)
        };

        spawn(future.instrument(Span::current())).await.unwrap()
    }

    /// Is the e2ee extension enabled for this sliding sync instance?
    #[cfg(feature = "e2e-encryption")]
    fn is_e2ee_enabled(&self) -> bool {
        self.inner.sticky.read().unwrap().data().extensions.e2ee.enabled == Some(true)
    }

    #[cfg(not(feature = "e2e-encryption"))]
    fn is_e2ee_enabled(&self) -> bool {
        false
    }

    /// Should we process the room's subpart of a response?
    async fn must_process_rooms_response(&self) -> bool {
        // We consider that we must, if there's any room subscription or there's any
        // list.
        !self.inner.sticky.read().unwrap().data().room_subscriptions.is_empty()
            || !self.inner.lists.read().await.is_empty()
    }

    #[instrument(skip_all, fields(pos, conn_id = self.inner.id))]
    async fn sync_once(&self) -> Result<UpdateSummary> {
        let (request, request_config, position_guard) =
            self.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // Send the request, kaboom.
        let summaries = self.send_sync_request(request, request_config, position_guard).await?;

        // Notify a new sync was received
        self.inner.client.inner.sync_beat.notify(usize::MAX);

        Ok(summaries)
    }

    /// Create a _new_ Sliding Sync sync loop.
    ///
    /// This method returns a `Stream`, which will send requests and will handle
    /// responses automatically. Lists and rooms are updated automatically.
    ///
    /// This function returns `Ok(…)` if everything went well, otherwise it will
    /// return `Err(…)`. An `Err` will _always_ lead to the `Stream`
    /// termination.
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(name = "sync_stream", skip_all, fields(conn_id = self.inner.id, with_e2ee = self.is_e2ee_enabled()))]
    pub fn sync(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        debug!("Starting sync stream");

        let mut internal_channel_receiver = self.inner.internal_channel.subscribe();

        stream! {
            loop {
                debug!("Sync stream is running");

                select! {
                    biased;

                    internal_message = internal_channel_receiver.recv() => {
                        use SlidingSyncInternalMessage::*;

                        debug!(?internal_message, "Sync stream has received an internal message");

                        match internal_message {
                            Err(_) | Ok(SyncLoopStop) => {
                                break;
                            }

                            Ok(SyncLoopSkipOverCurrentIteration) => {
                                continue;
                            }
                        }
                    }

                    update_summary = self.sync_once() => {
                        match update_summary {
                            Ok(updates) => {
                                yield Ok(updates);
                            }

                            // Here, errors we **cannot** ignore, and that must stop the sync loop.
                            Err(error) => {
                                if error.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                                    // The Sliding Sync session has expired. Let's reset `pos` and sticky parameters.
                                    self.expire_session().await;
                                }

                                yield Err(error);

                                // Terminates the loop, and terminates the stream.
                                break;
                            }
                        }
                    }
                }
            }

            debug!("Sync stream has exited.");
        }
    }

    /// Force to stop the sync loop ([`Self::sync`]) if it's running.
    ///
    /// Usually, dropping the `Stream` returned by [`Self::sync`] should be
    /// enough to “stop” it, but depending of how this `Stream` is used, it
    /// might not be obvious to drop it immediately (thinking of using this API
    /// over FFI; the foreign-language might not be able to drop a value
    /// immediately). Thus, calling this method will ensure that the sync loop
    /// stops gracefully and as soon as it returns.
    pub fn stop_sync(&self) -> Result<()> {
        Ok(self.inner.internal_channel_send(SlidingSyncInternalMessage::SyncLoopStop)?)
    }

    /// Expire the current Sliding Sync session on the client-side.
    ///
    /// Expiring a Sliding Sync session means: resetting `pos`. It also resets
    /// sticky parameters.
    ///
    /// This should only be used when it's clear that this session was about to
    /// expire anyways, and should be used only in very specific cases (e.g.
    /// multiple sliding syncs being run in parallel, and one of them has
    /// expired).
    ///
    /// This method **MUST** be called when the sync loop is stopped.
    #[doc(hidden)]
    pub async fn expire_session(&self) {
        info!("Session expired; resetting `pos` and sticky parameters");

        {
            let mut position = self.inner.position.lock().await;
            position.pos = None;

            if let Err(err) = self.cache_to_storage(&position).await {
                error!(
                    "couldn't invalidate sliding sync frozen state when expiring session: {err}"
                );
            }
        }

        {
            let mut sticky = self.inner.sticky.write().unwrap();

            // Clear all room subscriptions: we don't want to resend all room subscriptions
            // when the session will restart.
            sticky.data_mut().room_subscriptions.clear();
        }

        self.inner.lists.read().await.values().for_each(|list| list.invalidate_sticky_data());
    }
}

impl SlidingSyncInner {
    /// Send a message over the internal channel.
    #[instrument]
    fn internal_channel_send(&self, message: SlidingSyncInternalMessage) -> Result<(), Error> {
        self.internal_channel.send(message).map(|_| ()).map_err(|_| Error::InternalChannelIsBroken)
    }

    /// Send a message over the internal channel if there is a receiver, i.e. if
    /// the sync loop is running.
    #[instrument]
    fn internal_channel_send_if_possible(&self, message: SlidingSyncInternalMessage) {
        // If there is no receiver, the send will fail, but that's OK here.
        let _ = self.internal_channel.send(message);
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum SlidingSyncInternalMessage {
    /// Instruct the sync loop to stop.
    SyncLoopStop,

    /// Instruct the sync loop to skip over any remaining work in its iteration,
    /// and to jump to the next iteration.
    SyncLoopSkipOverCurrentIteration,
}

#[cfg(any(test, feature = "testing"))]
impl SlidingSync {
    /// Set a new value for `pos`.
    pub async fn set_pos(&self, new_pos: String) {
        let mut position_lock = self.inner.position.lock().await;
        position_lock.pos = Some(new_pos);
    }

    /// Read the static extension configuration for this Sliding Sync.
    ///
    /// Note: this is not the next content of the sticky parameters, but rightly
    /// the static configuration that was set during creation of this
    /// Sliding Sync.
    pub fn extensions_config(&self) -> http::request::Extensions {
        let sticky = self.inner.sticky.read().unwrap();
        sticky.data().extensions.clone()
    }
}

#[derive(Clone, Debug)]
pub(super) struct SlidingSyncPositionMarkers {
    /// An ephemeral position in the current stream, as received from the
    /// previous `/sync` response, or `None` for the first request.
    ///
    /// Should not be persisted.
    pos: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSyncPos {
    #[serde(skip_serializing_if = "Option::is_none")]
    pos: Option<String>,
}

/// A summary of the updates received after a sync (like in
/// [`SlidingSync::sync`]).
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The names of the lists that have seen an update.
    pub lists: Vec<String>,
    /// The rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

/// A very basic bool-ish enum to represent the state of a
/// [`http::request::RoomSubscription`]. A `RoomSubscription` that has been sent
/// once should ideally not being sent again, to mostly save bandwidth.
#[derive(Debug, Default)]
enum RoomSubscriptionState {
    /// The `RoomSubscription` has not been sent or received correctly from the
    /// server, i.e. the `RoomSubscription` —which is part of the sticky
    /// parameters— has not been committed.
    #[default]
    Pending,

    /// The `RoomSubscription` has been sent and received correctly by the
    /// server.
    Applied,
}

/// The set of sticky parameters owned by the `SlidingSyncInner` instance, and
/// sent in the request.
#[derive(Debug)]
pub(super) struct SlidingSyncStickyParameters {
    /// Room subscriptions, i.e. rooms that may be out-of-scope of all lists
    /// but one wants to receive updates.
    room_subscriptions:
        BTreeMap<OwnedRoomId, (RoomSubscriptionState, http::request::RoomSubscription)>,

    /// The intended state of the extensions being supplied to sliding /sync
    /// calls.
    extensions: http::request::Extensions,
}

impl SlidingSyncStickyParameters {
    /// Create a new set of sticky parameters.
    pub fn new(
        room_subscriptions: BTreeMap<OwnedRoomId, http::request::RoomSubscription>,
        extensions: http::request::Extensions,
    ) -> Self {
        Self {
            room_subscriptions: room_subscriptions
                .into_iter()
                .map(|(room_id, room_subscription)| {
                    (room_id, (RoomSubscriptionState::Pending, room_subscription))
                })
                .collect(),
            extensions,
        }
    }
}

impl StickyData for SlidingSyncStickyParameters {
    type Request = http::Request;

    fn apply(&self, request: &mut Self::Request) {
        request.room_subscriptions = self
            .room_subscriptions
            .iter()
            .filter(|(_, (state, _))| matches!(state, RoomSubscriptionState::Pending))
            .map(|(room_id, (_, room_subscription))| (room_id.clone(), room_subscription.clone()))
            .collect();
        request.extensions = self.extensions.clone();
    }

    fn on_commit(&mut self) {
        // All room subscriptions are marked as `Applied`.
        for (state, _room_subscription) in self.room_subscriptions.values_mut() {
            if matches!(state, RoomSubscriptionState::Pending) {
                *state = RoomSubscriptionState::Applied;
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
#[allow(clippy::dbg_macro)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::ready,
        ops::Not,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use assert_matches::assert_matches;
    use event_listener::Listener;
    use futures_util::{future::join_all, pin_mut, StreamExt};
    use matrix_sdk_base::{RequestedRequiredStates, RoomMemberships};
    use matrix_sdk_test::{async_test, event_factory::EventFactory, ALICE};
    use ruma::{
        api::client::error::ErrorKind,
        assign,
        events::{direct::DirectEvent, room::member::MembershipState},
        owned_room_id, room_id,
        serde::Raw,
        uint, OwnedRoomId, TransactionId,
    };
    use serde::Deserialize;
    use serde_json::json;
    use wiremock::{
        http::Method, matchers::method, Match, Mock, MockServer, Request, ResponseTemplate,
    };

    use super::{
        http,
        sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager},
        SlidingSync, SlidingSyncList, SlidingSyncListBuilder, SlidingSyncMode,
        SlidingSyncStickyParameters,
    };
    use crate::{
        sliding_sync::cache::restore_sliding_sync_state,
        test_utils::{logged_in_client, mocks::MatrixMockServer},
        Client, Result,
    };

    #[derive(Copy, Clone)]
    struct SlidingSyncMatcher;

    impl Match for SlidingSyncMatcher {
        fn matches(&self, request: &Request) -> bool {
            request.url.path() == "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"
                && request.method == Method::POST
        }
    }

    async fn new_sliding_sync(
        lists: Vec<SlidingSyncListBuilder>,
    ) -> Result<(MockServer, SlidingSync)> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut sliding_sync_builder = client.sliding_sync("test-slidingsync")?;

        for list in lists {
            sliding_sync_builder = sliding_sync_builder.add_list(list);
        }

        let sliding_sync = sliding_sync_builder.build().await?;

        Ok((server, sliding_sync))
    }

    #[async_test]
    async fn test_subscribe_to_rooms() -> Result<()> {
        let (server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        let stream = sliding_sync.sync();
        pin_mut!(stream);

        let room_id_0 = room_id!("!r0:bar.org");
        let room_id_1 = room_id!("!r1:bar.org");
        let room_id_2 = room_id!("!r2:bar.org");

        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "pos": "1",
                    "lists": {},
                    "rooms": {
                        room_id_0: {
                            "name": "Room #0",
                            "initial": true,
                        },
                        room_id_1: {
                            "name": "Room #1",
                            "initial": true,
                        },
                        room_id_2: {
                            "name": "Room #2",
                            "initial": true,
                        },
                    }
                })))
                .mount_as_scoped(&server)
                .await;

            let _ = stream.next().await.unwrap()?;
        }

        let room0 = sliding_sync.inner.client.get_room(room_id_0).unwrap();

        // Members aren't synced.
        // We need to make them synced, so that we can test that subscribing to a room
        // make members not synced. That's a desired feature.
        assert!(room0.are_members_synced().not());

        {
            struct MemberMatcher(OwnedRoomId);

            impl Match for MemberMatcher {
                fn matches(&self, request: &Request) -> bool {
                    request.url.path()
                        == format!("/_matrix/client/r0/rooms/{room_id}/members", room_id = self.0)
                        && request.method == Method::GET
                }
            }

            let _mock_guard = Mock::given(MemberMatcher(room_id_0.to_owned()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "chunk": [],
                })))
                .mount_as_scoped(&server)
                .await;

            assert_matches!(room0.request_members().await, Ok(()));
        }

        // Members are now synced! We can start subscribing and see how it goes.
        assert!(room0.are_members_synced());

        sliding_sync.subscribe_to_rooms(&[room_id_0, room_id_1], None, true);

        // OK, we have subscribed to some rooms. Let's check on `room0` if members are
        // now marked as not synced.
        assert!(room0.are_members_synced().not());

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.contains_key(room_id_0));
            assert!(room_subscriptions.contains_key(room_id_1));
            assert!(!room_subscriptions.contains_key(room_id_2));
        }

        // Subscribing to the same room doesn't reset the member sync state.

        {
            struct MemberMatcher(OwnedRoomId);

            impl Match for MemberMatcher {
                fn matches(&self, request: &Request) -> bool {
                    request.url.path()
                        == format!("/_matrix/client/r0/rooms/{room_id}/members", room_id = self.0)
                        && request.method == Method::GET
                }
            }

            let _mock_guard = Mock::given(MemberMatcher(room_id_0.to_owned()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "chunk": [],
                })))
                .mount_as_scoped(&server)
                .await;

            assert_matches!(room0.request_members().await, Ok(()));
        }

        // Members are synced, good, good.
        assert!(room0.are_members_synced());

        sliding_sync.subscribe_to_rooms(&[room_id_0], None, false);

        // Members are still synced: because we have already subscribed to the
        // room, the members aren't marked as unsynced.
        assert!(room0.are_members_synced());

        Ok(())
    }

    #[async_test]
    async fn test_room_subscriptions_are_reset_when_session_expires() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        let room_id_0 = room_id!("!r0:bar.org");
        let room_id_1 = room_id!("!r1:bar.org");
        let room_id_2 = room_id!("!r2:bar.org");

        // Subscribe to two rooms.
        sliding_sync.subscribe_to_rooms(&[room_id_0, room_id_1], None, false);

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.contains_key(room_id_0));
            assert!(room_subscriptions.contains_key(room_id_1));
            assert!(room_subscriptions.contains_key(room_id_2).not());
        }

        // Subscribe to one more room.
        sliding_sync.subscribe_to_rooms(&[room_id_2], None, false);

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.contains_key(room_id_0));
            assert!(room_subscriptions.contains_key(room_id_1));
            assert!(room_subscriptions.contains_key(room_id_2));
        }

        // Suddenly, the session expires!
        sliding_sync.expire_session().await;

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.is_empty());
        }

        // Subscribe to one room again.
        sliding_sync.subscribe_to_rooms(&[room_id_2], None, false);

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.contains_key(room_id_0).not());
            assert!(room_subscriptions.contains_key(room_id_1).not());
            assert!(room_subscriptions.contains_key(room_id_2));
        }

        Ok(())
    }

    #[async_test]
    async fn test_add_list() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        let _stream = sliding_sync.sync();
        pin_mut!(_stream);

        sliding_sync
            .add_list(
                SlidingSyncList::builder("bar")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(50..=60)),
            )
            .await?;

        let lists = sliding_sync.inner.lists.read().await;

        assert!(lists.contains_key("foo"));
        assert!(lists.contains_key("bar"));

        // this test also ensures that Tokio is not panicking when calling `add_list`.

        Ok(())
    }

    #[test]
    fn test_sticky_parameters_api_invalidated_flow() {
        let r0 = room_id!("!r0.matrix.org");
        let r1 = room_id!("!r1:matrix.org");

        let mut room_subscriptions = BTreeMap::new();
        room_subscriptions.insert(r0.to_owned(), Default::default());

        // At first it's invalidated.
        let mut sticky = SlidingSyncStickyManager::new(SlidingSyncStickyParameters::new(
            room_subscriptions,
            Default::default(),
        ));
        assert!(sticky.is_invalidated());

        // Then when we create a request, the sticky parameters are applied.
        let txn_id: &TransactionId = "tid123".into();

        let mut request = http::Request::default();
        request.txn_id = Some(txn_id.to_string());

        sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

        assert!(request.txn_id.is_some());
        assert_eq!(request.room_subscriptions.len(), 1);
        assert!(request.room_subscriptions.contains_key(r0));

        let tid = request.txn_id.unwrap();

        sticky.maybe_commit(tid.as_str().into());
        assert!(!sticky.is_invalidated());

        // Applying new parameters will invalidate again.
        sticky
            .data_mut()
            .room_subscriptions
            .insert(r1.to_owned(), (Default::default(), Default::default()));
        assert!(sticky.is_invalidated());

        // Committing with the wrong transaction id will keep it invalidated.
        sticky.maybe_commit("wrong tid today, my love has gone away 🎵".into());
        assert!(sticky.is_invalidated());

        // Restarting a request will only remember the last generated transaction id.
        let txn_id1: &TransactionId = "tid456".into();
        let mut request1 = http::Request::default();
        request1.txn_id = Some(txn_id1.to_string());
        sticky.maybe_apply(&mut request1, &mut LazyTransactionId::from_owned(txn_id1.to_owned()));

        assert!(sticky.is_invalidated());
        // The first room subscription has been applied to `request`, so it's not
        // reapplied here. It's a particular logic of `room_subscriptions`, it's not
        // related to the sticky design.
        assert_eq!(request1.room_subscriptions.len(), 1);
        assert!(request1.room_subscriptions.contains_key(r1));

        let txn_id2: &TransactionId = "tid789".into();
        let mut request2 = http::Request::default();
        request2.txn_id = Some(txn_id2.to_string());

        sticky.maybe_apply(&mut request2, &mut LazyTransactionId::from_owned(txn_id2.to_owned()));
        assert!(sticky.is_invalidated());
        // `request2` contains `r1` because the sticky parameters have not been
        // committed, so it's still marked as pending.
        assert_eq!(request2.room_subscriptions.len(), 1);
        assert!(request2.room_subscriptions.contains_key(r1));

        // Here we commit with the not most-recent TID, so it keeps the invalidated
        // status.
        sticky.maybe_commit(txn_id1);
        assert!(sticky.is_invalidated());

        // But here we use the latest TID, so the commit is effective.
        sticky.maybe_commit(txn_id2);
        assert!(!sticky.is_invalidated());
    }

    #[test]
    fn test_room_subscriptions_are_sticky() {
        let r0 = room_id!("!r0.matrix.org");
        let r1 = room_id!("!r1:matrix.org");

        let mut sticky = SlidingSyncStickyManager::new(SlidingSyncStickyParameters::new(
            BTreeMap::new(),
            Default::default(),
        ));

        // A room subscription is added, applied, and committed.
        {
            // Insert `r0`.
            sticky
                .data_mut()
                .room_subscriptions
                .insert(r0.to_owned(), (Default::default(), Default::default()));

            // Then the sticky parameters are applied.
            let txn_id: &TransactionId = "tid0".into();
            let mut request = http::Request::default();
            request.txn_id = Some(txn_id.to_string());

            sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

            assert!(request.txn_id.is_some());
            assert_eq!(request.room_subscriptions.len(), 1);
            assert!(request.room_subscriptions.contains_key(r0));

            // Then the sticky parameters are committed.
            let tid = request.txn_id.unwrap();

            sticky.maybe_commit(tid.as_str().into());
        }

        // A room subscription is added, applied, but NOT committed.
        {
            // Insert `r1`.
            sticky
                .data_mut()
                .room_subscriptions
                .insert(r1.to_owned(), (Default::default(), Default::default()));

            // Then the sticky parameters are applied.
            let txn_id: &TransactionId = "tid1".into();
            let mut request = http::Request::default();
            request.txn_id = Some(txn_id.to_string());

            sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

            assert!(request.txn_id.is_some());
            assert_eq!(request.room_subscriptions.len(), 1);
            // `r0` is not present, it's only `r1`.
            assert!(request.room_subscriptions.contains_key(r1));

            // Then the sticky parameters are NOT committed.
            // It can happen if the request has failed to be sent for example,
            // or if the response didn't match.
        }

        // A previously added room subscription is re-added, applied, and committed.
        {
            // Then the sticky parameters are applied.
            let txn_id: &TransactionId = "tid2".into();
            let mut request = http::Request::default();
            request.txn_id = Some(txn_id.to_string());

            sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

            assert!(request.txn_id.is_some());
            assert_eq!(request.room_subscriptions.len(), 1);
            // `r0` is not present, it's only `r1`.
            assert!(request.room_subscriptions.contains_key(r1));

            // Then the sticky parameters are committed.
            let tid = request.txn_id.unwrap();

            sticky.maybe_commit(tid.as_str().into());
        }

        // All room subscriptions have been committed.
        {
            // Then the sticky parameters are applied.
            let txn_id: &TransactionId = "tid3".into();
            let mut request = http::Request::default();
            request.txn_id = Some(txn_id.to_string());

            sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

            assert!(request.txn_id.is_some());
            // All room subscriptions have been sent.
            assert!(request.room_subscriptions.is_empty());
        }
    }

    #[test]
    fn test_extensions_are_sticky() {
        let mut extensions = http::request::Extensions::default();
        extensions.account_data.enabled = Some(true);

        // At first it's invalidated.
        let mut sticky = SlidingSyncStickyManager::new(SlidingSyncStickyParameters::new(
            Default::default(),
            extensions,
        ));

        assert!(sticky.is_invalidated(), "invalidated because of non default parameters");

        // `StickyParameters::new` follows its caller's intent when it comes to e2ee and
        // to-device.
        let extensions = &sticky.data().extensions;
        assert_eq!(extensions.e2ee.enabled, None);
        assert_eq!(extensions.to_device.enabled, None);
        assert_eq!(extensions.to_device.since, None);

        // What the user explicitly enabled is… enabled.
        assert_eq!(extensions.account_data.enabled, Some(true));

        let txn_id: &TransactionId = "tid123".into();
        let mut request = http::Request::default();
        request.txn_id = Some(txn_id.to_string());
        sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));
        assert!(sticky.is_invalidated());
        assert_eq!(request.extensions.to_device.enabled, None);
        assert_eq!(request.extensions.to_device.since, None);
        assert_eq!(request.extensions.e2ee.enabled, None);
        assert_eq!(request.extensions.account_data.enabled, Some(true));
    }

    #[async_test]
    async fn test_sticky_extensions_plus_since() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sync = client
            .sliding_sync("test-slidingsync")?
            .add_list(SlidingSyncList::builder("new_list"))
            .build()
            .await?;

        // No extensions have been explicitly enabled here.
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.to_device.enabled, None);
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.e2ee.enabled, None);
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.account_data.enabled, None);

        // Now enable e2ee and to-device.
        let sync = client
            .sliding_sync("test-slidingsync")?
            .add_list(SlidingSyncList::builder("new_list"))
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        // Even without a since token, the first request will contain the extensions
        // configuration, at least.
        let txn_id = TransactionId::new();
        let (request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        assert_eq!(request.extensions.e2ee.enabled, Some(true));
        assert_eq!(request.extensions.to_device.enabled, Some(true));
        assert!(request.extensions.to_device.since.is_none());

        {
            // Committing with another transaction id doesn't validate anything.
            let mut sticky = sync.inner.sticky.write().unwrap();
            assert!(sticky.is_invalidated());
            sticky.maybe_commit(
                "hopefully the rng won't generate this very specific transaction id".into(),
            );
            assert!(sticky.is_invalidated());
        }

        // Regenerating a request will yield the same one.
        let txn_id2 = TransactionId::new();
        let (request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id2.to_owned()))
            .await?;

        assert_eq!(request.extensions.e2ee.enabled, Some(true));
        assert_eq!(request.extensions.to_device.enabled, Some(true));
        assert!(request.extensions.to_device.since.is_none());

        assert!(txn_id != txn_id2, "the two requests must not share the same transaction id");

        {
            // Committing with the expected transaction id will validate it.
            let mut sticky = sync.inner.sticky.write().unwrap();
            assert!(sticky.is_invalidated());
            sticky.maybe_commit(txn_id2.as_str().into());
            assert!(!sticky.is_invalidated());
        }

        // The next request should contain no sticky parameters.
        let txn_id = TransactionId::new();
        let (request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;
        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());
        assert!(request.extensions.to_device.since.is_none());

        // If there's a to-device `since` token, we make sure we put the token
        // into the extension config. The rest doesn't need to be re-enabled due to
        // stickiness.
        let _since_token = "since";

        #[cfg(feature = "e2e-encryption")]
        {
            use matrix_sdk_base::crypto::store::Changes;
            if let Some(olm_machine) = &*client.olm_machine().await {
                olm_machine
                    .store()
                    .save_changes(Changes {
                        next_batch_token: Some(_since_token.to_owned()),
                        ..Default::default()
                    })
                    .await?;
            }
        }

        let txn_id = TransactionId::new();
        let (request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());

        #[cfg(feature = "e2e-encryption")]
        assert_eq!(request.extensions.to_device.since.as_deref(), Some(_since_token));

        Ok(())
    }

    // With MSC4186, with the `e2ee` extension enabled, if a request has no `pos`,
    // all the tracked users by the `OlmMachine` must be marked as dirty, i.e.
    // `/key/query` requests must be sent. See the code to see the details.
    //
    // This test is asserting that.
    #[async_test]
    #[cfg(feature = "e2e-encryption")]
    async fn test_no_pos_with_e2ee_marks_all_tracked_users_as_dirty() -> anyhow::Result<()> {
        use matrix_sdk_base::crypto::types::requests::{AnyIncomingResponse, AnyOutgoingRequest};
        use matrix_sdk_test::ruma_response_from_json;
        use ruma::user_id;

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let alice = user_id!("@alice:localhost");
        let bob = user_id!("@bob:localhost");
        let me = user_id!("@example:localhost");

        // Track and mark users are not dirty, so that we can check they are “dirty”
        // after that. Dirty here means that a `/key/query` must be sent.
        {
            let olm_machine = client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().unwrap();

            olm_machine.update_tracked_users([alice, bob]).await?;

            // Assert requests.
            let outgoing_requests = olm_machine.outgoing_requests().await?;

            assert_eq!(outgoing_requests.len(), 2);
            assert_matches!(outgoing_requests[0].request(), AnyOutgoingRequest::KeysUpload(_));
            assert_matches!(outgoing_requests[1].request(), AnyOutgoingRequest::KeysQuery(_));

            // Fake responses.
            olm_machine
                .mark_request_as_sent(
                    outgoing_requests[0].request_id(),
                    AnyIncomingResponse::KeysUpload(&ruma_response_from_json(&json!({
                        "one_time_key_counts": {}
                    }))),
                )
                .await?;

            olm_machine
                .mark_request_as_sent(
                    outgoing_requests[1].request_id(),
                    AnyIncomingResponse::KeysQuery(&ruma_response_from_json(&json!({
                        "device_keys": {
                            alice: {},
                            bob: {},
                        }
                    }))),
                )
                .await?;

            // Once more.
            let outgoing_requests = olm_machine.outgoing_requests().await?;

            assert_eq!(outgoing_requests.len(), 1);
            assert_matches!(outgoing_requests[0].request(), AnyOutgoingRequest::KeysQuery(_));

            olm_machine
                .mark_request_as_sent(
                    outgoing_requests[0].request_id(),
                    AnyIncomingResponse::KeysQuery(&ruma_response_from_json(&json!({
                        "device_keys": {
                            me: {},
                        }
                    }))),
                )
                .await?;

            // No more.
            let outgoing_requests = olm_machine.outgoing_requests().await?;

            assert!(outgoing_requests.is_empty());
        }

        let sync = client
            .sliding_sync("test-slidingsync")?
            .add_list(SlidingSyncList::builder("new_list"))
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        // First request: no `pos`.
        let txn_id = TransactionId::new();
        let (_request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        // Now, tracked users must be dirty.
        {
            let olm_machine = client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().unwrap();

            // Assert requests.
            let outgoing_requests = olm_machine.outgoing_requests().await?;

            assert_eq!(outgoing_requests.len(), 1);
            assert_matches!(
                outgoing_requests[0].request(),
                AnyOutgoingRequest::KeysQuery(request) => {
                    assert!(request.device_keys.contains_key(alice));
                    assert!(request.device_keys.contains_key(bob));
                    assert!(request.device_keys.contains_key(me));
                }
            );

            // Fake responses.
            olm_machine
                .mark_request_as_sent(
                    outgoing_requests[0].request_id(),
                    AnyIncomingResponse::KeysQuery(&ruma_response_from_json(&json!({
                        "device_keys": {
                            alice: {},
                            bob: {},
                            me: {},
                        }
                    }))),
                )
                .await?;
        }

        // Second request: with a `pos` this time.
        sync.set_pos("chocolat".to_owned()).await;

        let txn_id = TransactionId::new();
        let (_request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        // Tracked users are not marked as dirty.
        {
            let olm_machine = client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().unwrap();

            // Assert requests.
            let outgoing_requests = olm_machine.outgoing_requests().await?;

            assert!(outgoing_requests.is_empty());
        }

        Ok(())
    }

    #[async_test]
    async fn test_unknown_pos_resets_pos_and_sticky_parameters() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client
            .sliding_sync("test-slidingsync")?
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true) }),
            )
            .build()
            .await?;

        // First request asks to enable the extension.
        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
        assert!(request.extensions.to_device.enabled.is_some());

        let sync = sliding_sync.sync();
        pin_mut!(sync);

        // `pos` is `None` to start with.
        assert!(sliding_sync.inner.position.lock().await.pos.is_none());

        #[derive(Deserialize)]
        struct PartialRequest {
            txn_id: Option<String>,
        }

        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(|request: &Request| {
                    // Repeat the txn_id in the response, if set.
                    let request: PartialRequest = request.body_json().unwrap();

                    ResponseTemplate::new(200).set_body_json(json!({
                        "txn_id": request.txn_id,
                        "pos": "0",
                    }))
                })
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(next, Some(Ok(_update_summary)));

            // `pos` has been updated.
            assert_eq!(sliding_sync.inner.position.lock().await.pos, Some("0".to_owned()));
        }

        // Next request doesn't ask to enable the extension.
        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
        assert!(request.extensions.to_device.enabled.is_none());

        // Next request is successful.
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(|request: &Request| {
                    // Repeat the txn_id in the response, if set.
                    let request: PartialRequest = request.body_json().unwrap();

                    ResponseTemplate::new(200).set_body_json(json!({
                        "txn_id": request.txn_id,
                        "pos": "1",
                    }))
                })
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(next, Some(Ok(_update_summary)));

            // `pos` has been updated.
            assert_eq!(sliding_sync.inner.position.lock().await.pos, Some("1".to_owned()));
        }

        // Next request is successful despite it receives an already
        // received `pos` from the server.
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(|request: &Request| {
                    // Repeat the txn_id in the response, if set.
                    let request: PartialRequest = request.body_json().unwrap();

                    ResponseTemplate::new(200).set_body_json(json!({
                        "txn_id": request.txn_id,
                        "pos": "0", // <- already received!
                    }))
                })
                .up_to_n_times(1) // run this mock only once.
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(next, Some(Ok(_update_summary)));

            // `pos` has been updated.
            assert_eq!(sliding_sync.inner.position.lock().await.pos, Some("0".to_owned()));
        }

        // Stop responding with successful requests!
        //
        // When responding with `M_UNKNOWN_POS`, that regenerates the sticky parameters,
        // so they're reset. It also resets the `pos`.
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                    "error": "foo",
                    "errcode": "M_UNKNOWN_POS",
                })))
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;

            // The expected error is returned.
            assert_matches!(next, Some(Err(err)) if err.client_api_error_kind() == Some(&ErrorKind::UnknownPos));

            // `pos` has been reset.
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            // Next request asks to enable the extension again.
            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

            assert!(request.extensions.to_device.enabled.is_some());

            // `sync` has been stopped.
            assert!(sync.next().await.is_none());
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_sliding_sync_doesnt_remember_pos() -> Result<()> {
        let server = MockServer::start().await;

        #[derive(Deserialize)]
        struct PartialRequest {
            txn_id: Option<String>,
        }

        let server_pos = Arc::new(Mutex::new(0));
        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(move |request: &Request| {
                // Repeat the txn_id in the response, if set.
                let request: PartialRequest = request.body_json().unwrap();
                let pos = {
                    let mut pos = server_pos.lock().unwrap();
                    let prev = *pos;
                    *pos += 1;
                    prev
                };

                ResponseTemplate::new(200).set_body_json(json!({
                    "txn_id": request.txn_id,
                    "pos": pos.to_string(),
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client.sliding_sync("forgetful-sync")?.build().await?;

        // `pos` is `None` to start with.
        {
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert!(request.pos.is_none());
        }

        let sync = sliding_sync.sync();
        pin_mut!(sync);

        // Sync goes well, and then the position is saved both into the internal memory
        // and the database.
        let next = sync.next().await;
        assert_matches!(next, Some(Ok(_update_summary)));

        assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("0"));

        let restored_fields = restore_sliding_sync_state(&client, &sliding_sync.inner.storage_key)
            .await?
            .expect("must have restored fields");

        // While it has been saved into the database, it's not necessarily going to be
        // used later!
        assert_eq!(restored_fields.pos.as_deref(), Some("0"));

        // Now, even if we mess with the position stored in the database, the sliding
        // sync instance isn't configured to reload the stream position from the
        // database, so it won't be changed.
        {
            let other_sync = client.sliding_sync("forgetful-sync")?.build().await?;

            let mut position_guard = other_sync.inner.position.lock().await;
            position_guard.pos = Some("yolo".to_owned());

            other_sync.cache_to_storage(&position_guard).await?;
        }

        // It's still 0, not "yolo".
        {
            assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("0"));
            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert_eq!(request.pos.as_deref(), Some("0"));
        }

        // Recreating a sliding sync with the same ID doesn't preload the pos, if not
        // asked to.
        {
            let sliding_sync = client.sliding_sync("forgetful-sync")?.build().await?;
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_sliding_sync_does_remember_pos() -> Result<()> {
        let server = MockServer::start().await;

        #[derive(Deserialize)]
        struct PartialRequest {
            txn_id: Option<String>,
        }

        let server_pos = Arc::new(Mutex::new(0));
        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(move |request: &Request| {
                // Repeat the txn_id in the response, if set.
                let request: PartialRequest = request.body_json().unwrap();
                let pos = {
                    let mut pos = server_pos.lock().unwrap();
                    let prev = *pos;
                    *pos += 1;
                    prev
                };

                ResponseTemplate::new(200).set_body_json(json!({
                    "txn_id": request.txn_id,
                    "pos": pos.to_string(),
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client.sliding_sync("elephant-sync")?.share_pos().build().await?;

        // `pos` is `None` to start with.
        {
            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

            assert!(request.pos.is_none());
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());
        }

        let sync = sliding_sync.sync();
        pin_mut!(sync);

        // Sync goes well, and then the position is saved both into the internal memory
        // and the database.
        let next = sync.next().await;
        assert_matches!(next, Some(Ok(_update_summary)));

        assert_eq!(sliding_sync.inner.position.lock().await.pos, Some("0".to_owned()));

        let restored_fields = restore_sliding_sync_state(&client, &sliding_sync.inner.storage_key)
            .await?
            .expect("must have restored fields");

        // While it has been saved into the database, it's not necessarily going to be
        // used later!
        assert_eq!(restored_fields.pos.as_deref(), Some("0"));

        // Another process modifies the stream position under our feet...
        {
            let other_sync = client.sliding_sync("elephant-sync")?.build().await?;

            let mut position_guard = other_sync.inner.position.lock().await;
            position_guard.pos = Some("42".to_owned());

            other_sync.cache_to_storage(&position_guard).await?;
        }

        // It's alright, the next request will load it from the database.
        {
            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert_eq!(request.pos.as_deref(), Some("42"));
            assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("42"));
        }

        // Recreating a sliding sync with the same ID will reload it too.
        {
            let sliding_sync = client.sliding_sync("elephant-sync")?.share_pos().build().await?;
            assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("42"));

            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert_eq!(request.pos.as_deref(), Some("42"));
        }

        // Invalidating the session will remove the in-memory value AND the database
        // value.
        sliding_sync.expire_session().await;

        {
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert!(request.pos.is_none());
        }

        // And new sliding syncs with the same ID won't find it either.
        {
            let sliding_sync = client.sliding_sync("elephant-sync")?.share_pos().build().await?;
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert!(request.pos.is_none());
        }

        Ok(())
    }

    #[async_test]
    async fn test_stop_sync_loop() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        // Start the sync loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync loop is actually running.
        assert!(stream.next().await.is_some());

        // Stop the sync loop.
        sliding_sync.stop_sync()?;

        // The sync loop is actually stopped.
        assert!(stream.next().await.is_none());

        // Start a new sync loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync loop is actually running.
        assert!(stream.next().await.is_some());

        Ok(())
    }

    #[async_test]
    async fn test_process_read_receipts() -> Result<()> {
        let room = owned_room_id!("!pony:example.org");

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        client.event_cache().subscribe().unwrap();

        let sliding_sync = client
            .sliding_sync("test")?
            .with_receipt_extension(
                assign!(http::request::Receipts::default(), { enabled: Some(true) }),
            )
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=100)),
            )
            .build()
            .await?;

        // Initial state.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                rooms: BTreeMap::from([(
                    room.clone(),
                    http::response::Room::default(),
                )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        let server_response = assign!(http::Response::new("1".to_owned()), {
            extensions: assign!(http::response::Extensions::default(), {
                receipts: assign!(http::response::Receipts::default(), {
                    rooms: BTreeMap::from([
                        (
                            room.clone(),
                            Raw::from_json_string(
                                json!({
                                    "room_id": room,
                                    "type": "m.receipt",
                                    "content": {
                                        "$event:bar.org": {
                                            "m.read": {
                                                client.user_id().unwrap(): {
                                                    "ts": 1436451550,
                                                }
                                            }
                                        }
                                    }
                                })
                                .to_string(),
                            ).unwrap()
                        )
                    ])
                })
            })
        });

        let summary = {
            let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut pos_guard,
                    RequestedRequiredStates::default(),
                )
                .await?
        };

        assert!(summary.rooms.contains(&room));

        Ok(())
    }

    #[async_test]
    async fn test_process_marked_unread_room_account_data() -> Result<()> {
        let room_id = owned_room_id!("!unicorn:example.org");

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // Setup sliding sync with with one room and one list

        let sliding_sync = client
            .sliding_sync("test")?
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=100)),
            )
            .build()
            .await?;

        // Initial state.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                rooms: BTreeMap::from([(
                    room_id.clone(),
                    http::response::Room::default(),
                )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        // Simulate a response that only changes the marked unread state of the room to
        // true

        let server_response = make_mark_unread_response("1", room_id.clone(), true, false);

        let update_summary = {
            let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut pos_guard,
                    RequestedRequiredStates::default(),
                )
                .await?
        };

        // Check that the list list and entry received the update

        assert!(update_summary.rooms.contains(&room_id));

        let room = client.get_room(&room_id).unwrap();

        // Check the actual room data, this powers RoomInfo

        assert!(room.is_marked_unread());

        // Change it back to false and check if it updates

        let server_response = make_mark_unread_response("2", room_id.clone(), false, true);

        let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
        sliding_sync
            .handle_response(
                server_response.clone(),
                &mut pos_guard,
                RequestedRequiredStates::default(),
            )
            .await?;

        let room = client.get_room(&room_id).unwrap();

        assert!(!room.is_marked_unread());

        Ok(())
    }

    fn make_mark_unread_response(
        response_number: &str,
        room_id: OwnedRoomId,
        unread: bool,
        add_rooms_section: bool,
    ) -> http::Response {
        let rooms = if add_rooms_section {
            BTreeMap::from([(room_id.clone(), http::response::Room::default())])
        } else {
            BTreeMap::new()
        };

        let extensions = assign!(http::response::Extensions::default(), {
            account_data: assign!(http::response::AccountData::default(), {
                rooms: BTreeMap::from([
                    (
                        room_id,
                        vec![
                            Raw::from_json_string(
                                json!({
                                    "content": {
                                        "unread": unread
                                    },
                                    "type": "m.marked_unread"
                                })
                                .to_string(),
                            ).unwrap()
                        ]
                    )
                ])
            })
        });

        assign!(http::Response::new(response_number.to_owned()), { rooms: rooms, extensions: extensions })
    }

    #[async_test]
    async fn test_process_rooms_account_data() -> Result<()> {
        let room = owned_room_id!("!pony:example.org");

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client
            .sliding_sync("test")?
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=100)),
            )
            .build()
            .await?;

        // Initial state.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                rooms: BTreeMap::from([(
                    room.clone(),
                    http::response::Room::default(),
                )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        let server_response = assign!(http::Response::new("1".to_owned()), {
            extensions: assign!(http::response::Extensions::default(), {
                account_data: assign!(http::response::AccountData::default(), {
                    rooms: BTreeMap::from([
                        (
                            room.clone(),
                            vec![
                                Raw::from_json_string(
                                    json!({
                                        "content": {
                                            "tags": {
                                                "u.work": {
                                                    "order": 0.9
                                                }
                                            }
                                        },
                                        "type": "m.tag"
                                    })
                                    .to_string(),
                                ).unwrap()
                            ]
                        )
                    ])
                })
            })
        });
        let summary = {
            let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut pos_guard,
                    RequestedRequiredStates::default(),
                )
                .await?
        };

        assert!(summary.rooms.contains(&room));

        Ok(())
    }

    #[async_test]
    #[cfg(feature = "e2e-encryption")]
    async fn test_process_only_encryption_events() -> Result<()> {
        use ruma::OneTimeKeyAlgorithm;

        let room = owned_room_id!("!croissant:example.org");

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let server_response = assign!(http::Response::new("0".to_owned()), {
            rooms: BTreeMap::from([(
                room.clone(),
                assign!(http::response::Room::default(), {
                    name: Some("Croissants lovers".to_owned()),
                    timeline: Vec::new(),
                }),
            )]),

            extensions: assign!(http::response::Extensions::default(), {
                e2ee: assign!(http::response::E2EE::default(), {
                    device_one_time_keys_count: BTreeMap::from([(OneTimeKeyAlgorithm::SignedCurve25519, uint!(42))])
                }),
                to_device: Some(assign!(http::response::ToDevice::default(), {
                    next_batch: "to-device-token".to_owned(),
                })),
            })
        });

        // Don't process non-encryption events if the sliding sync is configured for
        // encryption only.

        let sliding_sync = client
            .sliding_sync("test")?
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        {
            let mut position_guard = sliding_sync.inner.position.clone().lock_owned().await;

            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut position_guard,
                    RequestedRequiredStates::default(),
                )
                .await?;
        }

        // E2EE has been properly handled.
        let uploaded_key_count = client.encryption().uploaded_key_count().await?;
        assert_eq!(uploaded_key_count, 42);

        {
            let olm_machine = &*client.olm_machine_for_testing().await;
            assert_eq!(
                olm_machine.as_ref().unwrap().store().next_batch_token().await?.as_deref(),
                Some("to-device-token")
            );
        }

        // Room events haven't.
        assert!(client.get_room(&room).is_none());

        // Conversely, only process room lists events if the sliding sync was configured
        // as so.
        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client
            .sliding_sync("test")?
            .add_list(SlidingSyncList::builder("thelist"))
            .build()
            .await?;

        {
            let mut position_guard = sliding_sync.inner.position.clone().lock_owned().await;

            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut position_guard,
                    RequestedRequiredStates::default(),
                )
                .await?;
        }

        // E2EE response has been ignored.
        let uploaded_key_count = client.encryption().uploaded_key_count().await?;
        assert_eq!(uploaded_key_count, 0);

        {
            let olm_machine = &*client.olm_machine_for_testing().await;
            assert_eq!(
                olm_machine.as_ref().unwrap().store().next_batch_token().await?.as_deref(),
                None
            );
        }

        // The room is now known.
        assert!(client.get_room(&room).is_some());

        // And it's also possible to set up both.
        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client
            .sliding_sync("test")?
            .add_list(SlidingSyncList::builder("thelist"))
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        {
            let mut position_guard = sliding_sync.inner.position.clone().lock_owned().await;

            sliding_sync
                .handle_response(
                    server_response.clone(),
                    &mut position_guard,
                    RequestedRequiredStates::default(),
                )
                .await?;
        }

        // E2EE has been properly handled.
        let uploaded_key_count = client.encryption().uploaded_key_count().await?;
        assert_eq!(uploaded_key_count, 42);

        {
            let olm_machine = &*client.olm_machine_for_testing().await;
            assert_eq!(
                olm_machine.as_ref().unwrap().store().next_batch_token().await?.as_deref(),
                Some("to-device-token")
            );
        }

        // The room is now known.
        assert!(client.get_room(&room).is_some());

        Ok(())
    }

    #[async_test]
    async fn test_lock_multiple_requests() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let pos = Arc::new(Mutex::new(0));
        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(move |_: &Request| {
                let mut pos = pos.lock().unwrap();
                *pos += 1;
                ResponseTemplate::new(200).set_body_json(json!({
                    "pos": pos.to_string(),
                    "lists": {},
                    "rooms": {}
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let sliding_sync = client
            .sliding_sync("test")?
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        // Spawn two requests in parallel. Before #2430, this lead to a deadlock and the
        // test would never terminate.
        let requests = join_all([sliding_sync.sync_once(), sliding_sync.sync_once()]);

        for result in requests.await {
            result?;
        }

        Ok(())
    }

    #[async_test]
    async fn test_aborted_request_doesnt_update_future_requests() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let pos = Arc::new(Mutex::new(0));
        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(move |_: &Request| {
                let mut pos = pos.lock().unwrap();
                *pos += 1;
                // Respond slowly enough that we can skip one iteration.
                ResponseTemplate::new(200)
                    .set_body_json(json!({
                        "pos": pos.to_string(),
                        "lists": {},
                        "rooms": {}
                    }))
                    .set_delay(Duration::from_secs(2))
            })
            .mount_as_scoped(&server)
            .await;

        let sliding_sync =
            client
                .sliding_sync("test")?
                .add_list(SlidingSyncList::builder("room-list").sync_mode(
                    SlidingSyncMode::new_growing(10).maximum_number_of_rooms_to_fetch(100),
                ))
                .add_list(
                    SlidingSyncList::builder("another-list")
                        .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10)),
                )
                .build()
                .await?;

        let stream = sliding_sync.sync();
        pin_mut!(stream);

        let cloned_sync = sliding_sync.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;

            cloned_sync
                .on_list("another-list", |list| {
                    list.set_sync_mode(SlidingSyncMode::new_selective().add_range(10..=20));
                    ready(())
                })
                .await;
        });

        assert_matches!(stream.next().await, Some(Ok(_)));

        sliding_sync.stop_sync().unwrap();

        assert_matches!(stream.next().await, None);

        let mut num_requests = 0;

        for request in server.received_requests().await.unwrap() {
            if !SlidingSyncMatcher.matches(&request) {
                continue;
            }

            let another_list_ranges = if num_requests == 0 {
                // First request
                json!([[0, 10]])
            } else {
                // Second request
                json!([[10, 20]])
            };

            num_requests += 1;
            assert!(num_requests <= 2, "more than one request hit the server");

            let json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

            if let Err(err) = assert_json_diff::assert_json_matches_no_panic(
                &json_value,
                &json!({
                    "conn_id": "test",
                    "lists": {
                        "room-list": {
                            "ranges": [[0, 9]],
                            "required_state": [
                                ["m.room.encryption", ""],
                                ["m.room.tombstone", ""]
                            ],
                        },
                        "another-list": {
                            "ranges": another_list_ranges,
                            "required_state": [
                                ["m.room.encryption", ""],
                                ["m.room.tombstone", ""]
                            ],
                        },
                    }
                }),
                assert_json_diff::Config::new(assert_json_diff::CompareMode::Inclusive),
            ) {
                dbg!(json_value);
                panic!("json differ: {err}");
            }
        }

        assert_eq!(num_requests, 2);

        Ok(())
    }

    #[async_test]
    async fn test_timeout_zero_list() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![]).await?;

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // Zero list means sliding sync is fully loaded, so there is a timeout to wait
        // on new update to pop.
        assert!(request.timeout.is_some());

        Ok(())
    }

    #[async_test]
    async fn test_timeout_one_list() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![
            SlidingSyncList::builder("foo").sync_mode(SlidingSyncMode::new_growing(10))
        ])
        .await?;

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // The list does not require a timeout.
        assert!(request.timeout.is_none());

        // Simulate a response.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                lists: BTreeMap::from([(
                    "foo".to_owned(),
                    assign!(http::response::List::default(), {
                        count: uint!(7),
                    })
                 )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // The list is now fully loaded, so it requires a timeout.
        assert!(request.timeout.is_some());

        Ok(())
    }

    #[async_test]
    async fn test_timeout_three_lists() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![
            SlidingSyncList::builder("foo").sync_mode(SlidingSyncMode::new_growing(10)),
            SlidingSyncList::builder("bar").sync_mode(SlidingSyncMode::new_paging(10)),
            SlidingSyncList::builder("baz")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10)),
        ])
        .await?;

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // Two lists don't require a timeout.
        assert!(request.timeout.is_none());

        // Simulate a response.
        {
            let server_response = assign!(http::Response::new("0".to_owned()), {
                lists: BTreeMap::from([(
                    "foo".to_owned(),
                    assign!(http::response::List::default(), {
                        count: uint!(7),
                    })
                 )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // One don't require a timeout.
        assert!(request.timeout.is_none());

        // Simulate a response.
        {
            let server_response = assign!(http::Response::new("1".to_owned()), {
                lists: BTreeMap::from([(
                    "bar".to_owned(),
                    assign!(http::response::List::default(), {
                        count: uint!(7),
                    })
                 )])
            });

            let _summary = {
                let mut pos_guard = sliding_sync.inner.position.clone().lock_owned().await;
                sliding_sync
                    .handle_response(
                        server_response.clone(),
                        &mut pos_guard,
                        RequestedRequiredStates::default(),
                    )
                    .await?
            };
        }

        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        // All lists require a timeout.
        assert!(request.timeout.is_some());

        Ok(())
    }

    #[async_test]
    async fn test_sync_beat_is_notified_on_sync_response() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pos": "0",
                "lists": {},
                "rooms": {}
            })))
            .mount_as_scoped(&server)
            .await;

        let sliding_sync = client
            .sliding_sync("test")?
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        let sliding_sync = Arc::new(sliding_sync);

        // Create the listener and perform a sync request
        let sync_beat_listener = client.inner.sync_beat.listen();
        sliding_sync.sync_once().await?;

        // The sync beat listener should be notified shortly after
        assert!(sync_beat_listener.wait_timeout(Duration::from_secs(1)).is_some());
        Ok(())
    }

    #[async_test]
    async fn test_sync_beat_is_not_notified_on_sync_failure() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(ResponseTemplate::new(404))
            .mount_as_scoped(&server)
            .await;

        let sliding_sync = client
            .sliding_sync("test")?
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true)}))
            .build()
            .await?;

        let sliding_sync = Arc::new(sliding_sync);

        // Create the listener and perform a sync request
        let sync_beat_listener = client.inner.sync_beat.listen();
        let sync_result = sliding_sync.sync_once().await;
        assert!(sync_result.is_err());

        // The sync beat listener won't be notified in this case
        assert!(sync_beat_listener.wait_timeout(Duration::from_secs(1)).is_none());

        Ok(())
    }

    #[async_test]
    async fn test_sync_lock_is_released_before_calling_handlers() -> Result<()> {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!mu5hr00m:example.org");

        let _sync_mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pos": "0",
                "lists": {},
                "extensions": {
                    "account_data": {
                        "global": [
                            {
                                "type": "m.direct",
                                "content": {
                                    "@de4dlockh0lmes:example.org": [
                                        "!mu5hr00m:example.org"
                                    ]
                                }
                            }
                        ]
                    }
                },
                "rooms": {
                    room_id: {
                        "name": "Mario Bros Fanbase Room",
                        "initial": true,
                    },
                }
            })))
            .mount_as_scoped(server.server())
            .await;

        let f = EventFactory::new().room(room_id);

        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(r"/_matrix/client/v3/rooms/.*/members"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunk": [
                    f.member(&ALICE).membership(MembershipState::Join).into_raw_timeline(),
                ]
            })))
            .mount(server.server())
            .await;

        let (tx, rx) = tokio::sync::oneshot::channel();

        let tx = Arc::new(Mutex::new(Some(tx)));
        client.add_event_handler(move |_: DirectEvent, client: Client| async move {
            // Try to run a /members query while in a event handler.
            let members =
                client.get_room(room_id).unwrap().members(RoomMemberships::JOIN).await.unwrap();
            assert_eq!(members.len(), 1);
            tx.lock().unwrap().take().expect("sender consumed multiple times").send(()).unwrap();
        });

        let sliding_sync = client
            .sliding_sync("test")?
            .add_list(SlidingSyncList::builder("thelist"))
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .build()
            .await?;

        tokio::time::timeout(Duration::from_secs(5), sliding_sync.sync_once())
            .await
            .expect("Sync did not complete in time")
            .expect("Sync failed");

        // Wait for the event handler to complete.
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("Event handler did not complete in time")
            .expect("Event handler failed");

        Ok(())
    }
}
