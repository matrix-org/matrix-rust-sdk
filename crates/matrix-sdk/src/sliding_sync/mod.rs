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
mod room;
mod sticky_parameters;
mod utils;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt::Debug,
    future::Future,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use async_stream::stream;
use futures_core::stream::Stream;
use matrix_sdk_common::{ring_buffer::RingBuffer, timer};
use ruma::{
    api::client::{
        error::ErrorKind,
        sync::sync_events::v4::{self, ExtensionsConfig},
    },
    assign, OwnedEventId, OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select, spawn,
    sync::{broadcast::Sender, Mutex as AsyncMutex, OwnedMutexGuard, RwLock as AsyncRwLock},
};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};
use url::Url;

pub use self::{builder::*, error::*, list::*, room::*};
use self::{
    cache::restore_sliding_sync_state,
    client::SlidingSyncResponseProcessor,
    sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager, StickyData},
    utils::JoinHandleExt as _,
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
    /// Used to distinguish different connections to the sliding sync proxy.
    id: String,

    /// Customize the sliding sync proxy URL.
    sliding_sync_proxy: Option<Url>,

    /// The HTTP Matrix client.
    client: Client,

    /// Long-polling timeout that appears the sliding sync proxy request.
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

    /// Past position markers.
    past_positions: StdRwLock<RingBuffer<SlidingSyncPositionMarkers>>,

    /// The lists of this Sliding Sync instance.
    lists: AsyncRwLock<BTreeMap<String, SlidingSyncList>>,

    /// The rooms details
    rooms: AsyncRwLock<BTreeMap<OwnedRoomId, SlidingSyncRoom>>,

    /// Request parameters that are sticky.
    sticky: StdRwLock<SlidingSyncStickyManager<SlidingSyncStickyParameters>>,

    /// Rooms to unsubscribe, see [`Self::room_subscriptions`].
    room_unsubscriptions: StdRwLock<BTreeSet<OwnedRoomId>>,

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

    /// Subscribe to a given room.
    ///
    /// If the associated `Room` exists, it will be marked as
    /// members are missing, so that it ensures to re-fetch all members.
    pub fn subscribe_to_room(&self, room_id: OwnedRoomId, settings: Option<v4::RoomSubscription>) {
        if let Some(room) = self.inner.client.get_room(&room_id) {
            room.mark_members_missing();
        }

        self.inner
            .sticky
            .write()
            .unwrap()
            .data_mut()
            .room_subscriptions
            .insert(room_id, settings.unwrap_or_default());

        self.inner.internal_channel_send_if_possible(
            SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
        );
    }

    /// Unsubscribe from a given room.
    pub fn unsubscribe_from_room(&self, room_id: OwnedRoomId) {
        // Note: we don't use `BTreeMap::remove` here, because that would require
        // mutable access thus calling `data_mut()`, which in turn would
        // invalidate the sticky parameters even if the `room_id` wasn't in the
        // mapping.

        // If there's a subscription…
        if self.inner.sticky.read().unwrap().data().room_subscriptions.contains_key(&room_id) {
            // Remove it…
            self.inner.sticky.write().unwrap().data_mut().room_subscriptions.remove(&room_id);
            // … then keep the unsubscription for the next request.
            self.inner.room_unsubscriptions.write().unwrap().insert(room_id);

            self.inner.internal_channel_send_if_possible(
                SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
            );
        }
    }

    /// Lookup a specific room
    pub async fn get_room(&self, room_id: &RoomId) -> Option<SlidingSyncRoom> {
        self.inner.rooms.read().await.get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.inner.rooms.blocking_read().len()
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

        let reloaded_rooms =
            list_builder.set_cached_and_reload(&self.inner.client, &self.inner.storage_key).await?;

        if !reloaded_rooms.is_empty() {
            let mut rooms = self.inner.rooms.write().await;

            for (key, frozen) in reloaded_rooms {
                rooms.entry(key).or_insert_with(|| {
                    SlidingSyncRoom::from_frozen(frozen, self.inner.client.clone())
                });
            }
        }

        self.add_list(list_builder).await
    }

    /// Lookup a set of rooms
    pub async fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.inner.rooms.read().await;

        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub async fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.inner.rooms.read().await.values().cloned().collect()
    }

    /// Handle the HTTP response.
    #[instrument(skip_all)]
    async fn handle_response(
        &self,
        mut sliding_sync_response: v4::Response,
        position: &mut SlidingSyncPositionMarkers,
    ) -> Result<UpdateSummary, crate::Error> {
        let pos = Some(sliding_sync_response.pos.clone());

        {
            debug!(
                pos = ?sliding_sync_response.pos,
                delta_token = ?sliding_sync_response.delta_token,
                "Update position markers`"
            );

            // Look up for this new `pos` in the past position markers.
            let past_positions = self.inner.past_positions.read().unwrap();

            // The `pos` received by the server has already been received in the past!
            if past_positions.iter().any(|position| position.pos == pos) {
                error!(
                    ?sliding_sync_response,
                    "Sliding Sync response has ALREADY been handled by the client in the past"
                );

                return Err(Error::ResponseAlreadyReceived { pos }.into());
            }
        }

        let must_process_rooms_response = self.must_process_rooms_response().await;

        // Compute `limited`, if we're interested in a room list query.
        if must_process_rooms_response {
            let known_rooms = self.inner.rooms.read().await;
            compute_limited(&known_rooms, &mut sliding_sync_response.rooms);
        }

        // Transform a Sliding Sync Response to a `SyncResponse`.
        //
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `v4::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.

        let mut response_processor = SlidingSyncResponseProcessor::new(self.inner.client.clone());

        #[cfg(feature = "e2e-encryption")]
        if self.is_e2ee_enabled() {
            response_processor.handle_encryption(&sliding_sync_response.extensions).await?
        }

        // Only handle the room's subsection of the response, if this sliding sync was
        // configured to do so. That's because even when not requesting it,
        // sometimes the current (2023-07-20) proxy will forward room events
        // unrelated to the current connection's parameters.
        //
        // NOTE: SS proxy workaround.
        if must_process_rooms_response {
            response_processor.handle_room_response(&sliding_sync_response).await?;
        }

        let mut sync_response = response_processor.process_and_take_response().await?;

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
                let mut rooms_map = self.inner.rooms.write().await;

                let mut updated_rooms = Vec::with_capacity(sliding_sync_response.rooms.len());

                for (room_id, mut room_data) in sliding_sync_response.rooms.into_iter() {
                    // `sync_response` contains the rooms with decrypted events if any, so look at
                    // the timeline events here first if the room exists.
                    // Otherwise, let's look at the timeline inside the `sliding_sync_response`.
                    let timeline =
                        if let Some(joined_room) = sync_response.rooms.join.remove(&room_id) {
                            joined_room.timeline.events
                        } else {
                            room_data.timeline.drain(..).map(Into::into).collect()
                        };

                    match rooms_map.get_mut(&room_id) {
                        // The room existed before, let's update it.
                        Some(room) => {
                            room.update(room_data, timeline);
                        }

                        // First time we need this room, let's create it.
                        None => {
                            rooms_map.insert(
                                room_id.clone(),
                                SlidingSyncRoom::new(
                                    self.inner.client.clone(),
                                    room_id.clone(),
                                    room_data,
                                    timeline,
                                ),
                            );
                        }
                    }

                    updated_rooms.push(room_id);
                }

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

                for (name, updates) in sliding_sync_response.lists {
                    let Some(list) = lists.get_mut(&name) else {
                        error!("Response for list `{name}` - unknown to us; skipping");

                        continue;
                    };

                    let maximum_number_of_rooms: u32 =
                        updates.count.try_into().expect("failed to convert `count` to `u32`");

                    if list.update(maximum_number_of_rooms, &updates.ops, &updated_rooms)? {
                        updated_lists.push(name.clone());
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
        position.delta_token = sliding_sync_response.delta_token.clone();

        // Keep this position markers in memory, in case it pops from the server.
        let mut past_positions = self.inner.past_positions.write().unwrap();
        past_positions.push(position.clone());

        Ok(update_summary)
    }

    async fn generate_sync_request(
        &self,
        txn_id: &mut LazyTransactionId,
    ) -> Result<(
        v4::Request,
        RequestConfig,
        BTreeSet<OwnedRoomId>,
        OwnedMutexGuard<SlidingSyncPositionMarkers>,
    )> {
        // Collect requests for lists.
        let mut requests_lists = BTreeMap::new();

        {
            let lists = self.inner.lists.read().await;

            for (name, list) in lists.iter() {
                requests_lists.insert(name.clone(), list.next_request(txn_id)?);
            }
        }

        // Collect the `pos` and `delta_token`.
        //
        // Wait on the `position` mutex to be available. It means no request nor
        // response is running. The `position` mutex is released whether the response
        // has been fully handled successfully, in this case the `pos` is updated, or
        // the response handling has failed, in this case the `pos` hasn't been updated
        // and the same `pos` will be used for this new request.
        let mut position_guard = self.inner.position.clone().lock_owned().await;
        let delta_token = position_guard.delta_token.clone();

        let to_device_enabled =
            self.inner.sticky.read().unwrap().data().extensions.to_device.enabled == Some(true);

        let restored_fields = if self.inner.share_pos || to_device_enabled {
            let lists = self.inner.lists.read().await;
            restore_sliding_sync_state(&self.inner.client, &self.inner.storage_key, &lists).await?
        } else {
            None
        };

        // Update pos: either the one restored from the database, if any and the sliding
        // sync was configured so, or read it from the memory cache.
        let pos = if self.inner.share_pos {
            if let Some(fields) = &restored_fields {
                // Override the memory one with the database one, for consistency.
                if fields.pos != position_guard.pos {
                    info!("Pos from previous request ('{:?}') was different from pos in database ('{:?}').", position_guard.pos, fields.pos);
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

        // Collect other data.
        let room_unsubscriptions = self.inner.room_unsubscriptions.read().unwrap().clone();

        let mut request = assign!(v4::Request::new(), {
            conn_id: Some(self.inner.id.clone()),
            delta_token,
            pos,
            timeout: Some(self.inner.poll_timeout),
            lists: requests_lists,
            unsubscribe_rooms: room_unsubscriptions.iter().cloned().collect(),
        });

        // Apply sticky parameters, if needs be.
        self.inner.sticky.write().unwrap().maybe_apply(&mut request, txn_id);

        // Set the to-device token if the extension is enabled.
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
            RequestConfig::default().timeout(self.inner.poll_timeout + self.inner.network_timeout),
            room_unsubscriptions,
            position_guard,
        ))
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

    #[instrument(skip_all, fields(pos))]
    async fn sync_once(&self) -> Result<UpdateSummary> {
        let (request, request_config, requested_room_unsubscriptions, mut position_guard) =
            self.generate_sync_request(&mut LazyTransactionId::new()).await?;

        debug!("Sending request");

        // Prepare the request.
        let request = self.inner.client.send_with_homeserver(
            request,
            Some(request_config),
            self.inner.sliding_sync_proxy.as_ref().map(ToString::to_string),
        );

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
                let e2ee_uploads = spawn(async move {
                    if let Err(error) = client.send_outgoing_requests().await {
                        error!(?error, "Error while sending outoging E2EE requests");
                    }
                })
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

            // Room unsubscriptions have been received by the server. We can update the
            // unsubscriptions buffer. However, it would be an error to empty it entirely as
            // more unsubscriptions could have been inserted during the request/response
            // dance. So let's cherry-pick which unsubscriptions to remove.
            if !requested_room_unsubscriptions.is_empty() {
                let room_unsubscriptions = &mut *this.inner.room_unsubscriptions.write().unwrap();

                room_unsubscriptions
                    .retain(|room_id| !requested_room_unsubscriptions.contains(room_id));
            }

            // Handle the response.
            let updates = this.handle_response(response, &mut position_guard).await?;

            this.cache_to_storage(&position_guard).await?;

            // Release the position guard lock.
            // It means that other responses can be generated and then handled later.
            drop(position_guard);

            debug!("Done handling response");

            Ok(updates)
        };

        spawn(future.instrument(Span::current())).await.unwrap()
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

        let sync_span = Span::current();
        let mut internal_channel_receiver = self.inner.internal_channel.subscribe();

        stream! {
            loop {
                sync_span.in_scope(|| {
                    debug!("Sync stream is running");
                });

                select! {
                    biased;

                    internal_message = internal_channel_receiver.recv() => {
                        use SlidingSyncInternalMessage::*;

                        sync_span.in_scope(|| {
                            debug!(?internal_message, "Sync stream has received an internal message");
                        });

                        match internal_message {
                            Err(_) | Ok(SyncLoopStop) => {
                                break;
                            }

                            Ok(SyncLoopSkipOverCurrentIteration) => {
                                continue;
                            }
                        }
                    }

                    update_summary = self.sync_once().instrument(sync_span.clone()) => {
                        match update_summary {
                            Ok(updates) => {
                                yield Ok(updates);
                            }

                            // Here, errors we can safely ignore.
                            Err(crate::Error::SlidingSync(Error::ResponseAlreadyReceived { .. })) => {
                                continue;
                            }

                            // Here, errors we **cannot** ignore, and that must stop the sync loop.
                            Err(error) => {
                                if error.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                                    // The Sliding Sync session has expired. Let's reset `pos` and sticky parameters.
                                    sync_span.in_scope(|| async {
                                        self.expire_session().await;
                                    }).await;
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

    /// Expire the current Sliding Sync session.
    ///
    /// Expiring a Sliding Sync session means: resetting `pos`. It also cleans
    /// up the `past_positions`, and resets sticky parameters.
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

            let mut past_positions = self.inner.past_positions.write().unwrap();
            past_positions.clear();
        }

        // Force invalidation of all the sticky parameters.
        let _ = self.inner.sticky.write().unwrap().data_mut();

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
    /// Get a copy of the `pos` value.
    pub fn pos(&self) -> Option<String> {
        let position_lock = self.inner.position.blocking_lock();
        position_lock.pos.clone()
    }

    /// Set a new value for `pos`.
    pub fn set_pos(&self, new_pos: String) {
        let mut position_lock = self.inner.position.blocking_lock();
        position_lock.pos = Some(new_pos);
    }

    /// Get the URL to Sliding Sync.
    pub fn sliding_sync_proxy(&self) -> Option<Url> {
        self.inner.sliding_sync_proxy.clone()
    }

    /// Read the static extension configuration for this Sliding Sync.
    ///
    /// Note: this is not the next content of the sticky parameters, but rightly
    /// the static configuration that was set during creation of this
    /// Sliding Sync.
    pub fn extensions_config(&self) -> ExtensionsConfig {
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

    /// Server-provided opaque token that remembers what the last timeline and
    /// state events stored by the client were.
    ///
    /// If `None`, the server will send the full information for all the lists
    /// present in the request.
    delta_token: Option<String>,
}

/// Frozen bits of a Sliding Sync that are stored in the *state* store.
#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    /// Deprecated: prefer storing in the crypto store.
    #[serde(skip_serializing_if = "Option::is_none")]
    to_device_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_token: Option<String>,
}

impl FrozenSlidingSync {
    async fn new(position: &SlidingSyncPositionMarkers) -> Self {
        // The to-device token must be saved in the `FrozenCryptoSlidingSync` now.
        Self { delta_token: position.delta_token.clone(), to_device_since: None }
    }
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

/// The set of sticky parameters owned by the `SlidingSyncInner` instance, and
/// sent in the request.
#[derive(Debug)]
pub(super) struct SlidingSyncStickyParameters {
    /// Room subscriptions, i.e. rooms that may be out-of-scope of all lists
    /// but one wants to receive updates.
    room_subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,

    /// The intended state of the extensions being supplied to sliding /sync
    /// calls.
    extensions: ExtensionsConfig,
}

impl SlidingSyncStickyParameters {
    /// Create a new set of sticky parameters.
    pub fn new(
        room_subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
        extensions: ExtensionsConfig,
    ) -> Self {
        Self { room_subscriptions, extensions }
    }
}

impl StickyData for SlidingSyncStickyParameters {
    type Request = v4::Request;

    fn apply(&self, request: &mut Self::Request) {
        request.room_subscriptions = self.room_subscriptions.clone();
        request.extensions = self.extensions.clone();
    }
}

/// As of 2023-07-13, the sliding sync proxy doesn't provide us with `limited`
/// correctly, so we cheat and "correct" it using heuristics here.
/// TODO remove this workaround as soon as support of the `limited` flag is
/// properly implemented in the open-source proxy: https://github.com/matrix-org/sliding-sync/issues/197
// NOTE: SS proxy workaround.
fn compute_limited(
    local_rooms: &BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    remote_rooms: &mut BTreeMap<OwnedRoomId, v4::SlidingSyncRoom>,
) {
    for (room_id, remote_room) in remote_rooms {
        // Only rooms marked as initially loaded are subject to the fixup.
        let initial = remote_room.initial.unwrap_or(false);
        if !initial {
            continue;
        }

        if remote_room.limited {
            // If the room was already marked as limited, the server knew more than we do.
            continue;
        }

        let remote_events = &remote_room.timeline;
        if remote_events.is_empty() {
            trace!(%room_id, "no timeline updates in the response => not limited");
            continue;
        }

        let Some(local_room) = local_rooms.get(room_id) else {
            trace!(%room_id, "room isn't known locally => not limited");
            continue;
        };

        let local_events = local_room.timeline_queue();

        if local_events.is_empty() {
            trace!(%room_id, "local timeline had no events => not limited");
            continue;
        }

        // If the local room had some timeline events, consider it's a `limited` if
        // there's absolutely no overlap between the known events and the new
        // events in the timeline.

        // Gather all the known event IDs. Ignore events that don't have an event ID.
        let num_local_events = local_events.len();
        let local_events_with_ids: HashSet<OwnedEventId> =
            HashSet::from_iter(local_events.into_iter().filter_map(|event| event.event_id()));

        // There's overlap if, and only if, there's at least one event in the response's
        // timeline that matches an event id we've seen before.
        let mut num_remote_events_missing_ids = 0;
        let overlap = remote_events.iter().any(|remote_event| {
            if let Some(remote_event_id) =
                remote_event.get_field::<OwnedEventId>("event_id").ok().flatten()
            {
                local_events_with_ids.contains(&remote_event_id)
            } else {
                num_remote_events_missing_ids += 1;
                false
            }
        });

        remote_room.limited = !overlap;

        trace!(
            %room_id,
            num_events_response = remote_events.len(),
            num_local_events,
            num_local_events_with_ids = local_events_with_ids.len(),
            num_remote_events_missing_ids,
            room_limited = remote_room.limited,
            "done"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::ready,
        ops::Not,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use assert_matches::assert_matches;
    use futures_util::{future::join_all, pin_mut, StreamExt};
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::{
            error::ErrorKind,
            sync::sync_events::v4::{self, ExtensionsConfig, ToDeviceConfig},
        },
        assign, owned_room_id, room_id,
        serde::Raw,
        uint, DeviceKeyAlgorithm, OwnedRoomId, TransactionId,
    };
    use serde::Deserialize;
    use serde_json::json;
    use url::Url;
    use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

    use super::{
        compute_limited,
        sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager},
        FrozenSlidingSync, SlidingSync, SlidingSyncList, SlidingSyncListBuilder, SlidingSyncMode,
        SlidingSyncRoom, SlidingSyncStickyParameters,
    };
    use crate::{
        sliding_sync::cache::restore_sliding_sync_state, test_utils::logged_in_client, Result,
    };

    #[derive(Copy, Clone)]
    struct SlidingSyncMatcher;

    impl Match for SlidingSyncMatcher {
        fn matches(&self, request: &Request) -> bool {
            request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
                && request.method == Method::Post
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
    async fn test_subscribe_to_room() -> Result<()> {
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
                        && request.method == Method::Get
                }
            }

            let _mock_guard = Mock::given(MemberMatcher(room_id_0.to_owned()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "chunk": [],
                })))
                .mount_as_scoped(&server)
                .await;

            assert_matches!(room0.request_members().await, Ok(Some(_)));
        }

        // Members are now synced! We can start subscribing and see how it goes.
        assert!(room0.are_members_synced());

        sliding_sync.subscribe_to_room(room_id_0.to_owned(), None);
        sliding_sync.subscribe_to_room(room_id_1.to_owned(), None);

        // OK, we have subscribed to some rooms. Let's check on `room0` if members are
        // now marked as not synced.
        assert!(room0.are_members_synced().not());

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(room_subscriptions.contains_key(&room_id_0.to_owned()));
            assert!(room_subscriptions.contains_key(&room_id_1.to_owned()));
            assert!(!room_subscriptions.contains_key(&room_id_2.to_owned()));
        }

        sliding_sync.unsubscribe_from_room(room_id_0.to_owned());
        sliding_sync.unsubscribe_from_room(room_id_2.to_owned());

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = &sticky.data().room_subscriptions;

            assert!(!room_subscriptions.contains_key(&room_id_0.to_owned()));
            assert!(room_subscriptions.contains_key(&room_id_1.to_owned()));
            assert!(!room_subscriptions.contains_key(&room_id_2.to_owned()));

            let room_unsubscriptions = sliding_sync.inner.room_unsubscriptions.read().unwrap();

            assert!(room_unsubscriptions.contains(&room_id_0.to_owned()));
            assert!(!room_unsubscriptions.contains(&room_id_1.to_owned()));
            assert!(!room_unsubscriptions.contains(&room_id_2.to_owned()));
        }

        // this test also ensures that Tokio is not panicking when calling
        // `subscribe_to_room` and `unsubscribe_from_room`.

        Ok(())
    }

    #[async_test]
    async fn test_to_device_token_properly_cached() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        // FrozenSlidingSync doesn't contain the to_device_token anymore, as it's saved
        // in the crypto store since PR #2323.
        let position_guard = sliding_sync.inner.position.lock().await;
        let frozen = FrozenSlidingSync::new(&position_guard).await;
        assert!(frozen.to_device_since.is_none());

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
        let r0 = room_id!("!room:example.org");

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

        let mut request = v4::Request::default();
        request.txn_id = Some(txn_id.to_string());

        sticky.maybe_apply(&mut request, &mut LazyTransactionId::from_owned(txn_id.to_owned()));

        assert!(request.txn_id.is_some());
        assert_eq!(request.room_subscriptions.len(), 1);
        assert!(request.room_subscriptions.get(r0).is_some());

        let tid = request.txn_id.unwrap();

        sticky.maybe_commit(tid.as_str().into());
        assert!(!sticky.is_invalidated());

        // Applying new parameters will invalidate again.
        sticky
            .data_mut()
            .room_subscriptions
            .insert(room_id!("!r1:bar.org").to_owned(), Default::default());
        assert!(sticky.is_invalidated());

        // Committing with the wrong transaction id will keep it invalidated.
        sticky.maybe_commit("wrong tid today, my love has gone away 🎵".into());
        assert!(sticky.is_invalidated());

        // Restarting a request will only remember the last generated transaction id.
        let txn_id1: &TransactionId = "tid456".into();
        let mut request1 = v4::Request::default();
        request1.txn_id = Some(txn_id1.to_string());
        sticky.maybe_apply(&mut request1, &mut LazyTransactionId::from_owned(txn_id1.to_owned()));

        assert!(sticky.is_invalidated());
        assert_eq!(request1.room_subscriptions.len(), 2);

        let txn_id2: &TransactionId = "tid789".into();
        let mut request2 = v4::Request::default();
        request2.txn_id = Some(txn_id2.to_string());

        sticky.maybe_apply(&mut request2, &mut LazyTransactionId::from_owned(txn_id2.to_owned()));
        assert!(sticky.is_invalidated());
        assert_eq!(request2.room_subscriptions.len(), 2);

        // Here we commit with the not most-recent TID, so it keeps the invalidated
        // status.
        sticky.maybe_commit(txn_id1);
        assert!(sticky.is_invalidated());

        // But here we use the latest TID, so the commit is effective.
        sticky.maybe_commit(txn_id2);
        assert!(!sticky.is_invalidated());
    }

    #[test]
    fn test_extensions_are_sticky() {
        let mut extensions = ExtensionsConfig::default();
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
        assert_eq!(extensions.to_device.enabled, None,);
        assert_eq!(extensions.to_device.since, None,);

        // What the user explicitly enabled is... enabled.
        assert_eq!(extensions.account_data.enabled, Some(true),);

        let txn_id: &TransactionId = "tid123".into();
        let mut request = v4::Request::default();
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
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.to_device.enabled, None,);
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.e2ee.enabled, None);
        assert_eq!(sync.inner.sticky.read().unwrap().data().extensions.account_data.enabled, None);

        // Now enable e2ee and to-device.
        let sync = client
            .sliding_sync("test-slidingsync")?
            .add_list(SlidingSyncList::builder("new_list"))
            .with_to_device_extension(
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}))
            .build()
            .await?;

        // Even without a since token, the first request will contain the extensions
        // configuration, at least.
        let txn_id = TransactionId::new();
        let (request, _, _, _) = sync
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
        let (request, _, _, _) = sync
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
        let (request, _, _, _) = sync
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
        let (request, _, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());

        #[cfg(feature = "e2e-encryption")]
        assert_eq!(request.extensions.to_device.since.as_deref(), Some(_since_token));

        Ok(())
    }

    #[async_test]
    async fn test_unknown_pos_resets_pos_and_sticky_parameters() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sliding_sync = client
            .sliding_sync("test-slidingsync")?
            .with_to_device_extension(assign!(ToDeviceConfig::default(), { enabled: Some(true) }))
            .build()
            .await?;

        // First request asks to enable the extension.
        let (request, _, _, _) =
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

            // `past_positions` has been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 1);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
        }

        // Next request doesn't ask to enable the extension.
        let (request, _, _, _) =
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

            // `past_positions` has been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 2);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
            assert_eq!(past_positions.get(1).unwrap().pos, Some("1".to_owned()));
        }

        // Next request isn't successful because it receives an already
        // received `pos` from the server.
        {
            // First response with an already seen `pos`.
            let _mock_guard1 = Mock::given(SlidingSyncMatcher)
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

            // Second response with a new `pos`.
            let _mock_guard2 = Mock::given(SlidingSyncMatcher)
                .respond_with(|request: &Request| {
                    // Repeat the txn_id in the response, if set.
                    let request: PartialRequest = request.body_json().unwrap();

                    ResponseTemplate::new(200).set_body_json(json!({
                        "txn_id": request.txn_id,
                        "pos": "2", // <- new!
                    }))
                })
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(next, Some(Ok(_update_summary)));

            // `pos` has been updated.
            assert_eq!(sliding_sync.inner.position.lock().await.pos, Some("2".to_owned()));

            // `past_positions` has been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 3);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
            assert_eq!(past_positions.get(1).unwrap().pos, Some("1".to_owned()));
            assert_eq!(past_positions.get(2).unwrap().pos, Some("2".to_owned()));
        }

        // Stop responding with successful requests!
        //
        // When responding with M_UNKNOWN_POS, that regenerates the sticky parameters,
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

            // `past_positions` has been reset.
            assert!(sliding_sync.inner.past_positions.read().unwrap().is_empty());

            // Next request asks to enable the extension again.
            let (request, _, _, _) =
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

            let (request, _, _, _) =
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

        let restored_fields = restore_sliding_sync_state(
            &client,
            &sliding_sync.inner.storage_key,
            &*sliding_sync.inner.lists.read().await,
        )
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
            let (request, _, _, _) =
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
            let (request, _, _, _) =
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

        let restored_fields = restore_sliding_sync_state(
            &client,
            &sliding_sync.inner.storage_key,
            &*sliding_sync.inner.lists.read().await,
        )
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
            let (request, _, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert_eq!(request.pos.as_deref(), Some("42"));
            assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("42"));
        }

        // Recreating a sliding sync with the same ID will reload it too.
        {
            let sliding_sync = client.sliding_sync("elephant-sync")?.share_pos().build().await?;
            assert_eq!(sliding_sync.inner.position.lock().await.pos.as_deref(), Some("42"));

            let (request, _, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert_eq!(request.pos.as_deref(), Some("42"));
        }

        // Invalidating the session will remove the in-memory value AND the database
        // value.
        sliding_sync.expire_session().await;

        {
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            let (request, _, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
            assert!(request.pos.is_none());
        }

        // And new sliding syncs with the same ID won't find it either.
        {
            let sliding_sync = client.sliding_sync("elephant-sync")?.share_pos().build().await?;
            assert!(sliding_sync.inner.position.lock().await.pos.is_none());

            let (request, _, _, _) =
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
    async fn test_sliding_sync_proxy_url() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        {
            // A server that doesn't expose a sliding sync proxy gets and transmits none, by
            // default.
            let sync = client.sliding_sync("no-proxy")?.build().await?;

            assert!(sync.sliding_sync_proxy().is_none());
        }

        {
            // The sliding sync builder can be used to customize a proxy, though.
            let url = Url::parse("https://bar.matrix/").unwrap();
            let sync =
                client.sliding_sync("own-proxy")?.sliding_sync_proxy(url.clone()).build().await?;
            assert_eq!(sync.sliding_sync_proxy(), Some(url));
        }

        // Set the client's proxy, that will be inherited by sliding sync.
        let url = Url::parse("https://foo.matrix/").unwrap();
        client.set_sliding_sync_proxy(Some(url.clone()));

        {
            // The sliding sync inherits the client's sliding sync proxy URL.
            let sync = client.sliding_sync("client-proxy")?.build().await?;
            assert_eq!(sync.sliding_sync_proxy(), Some(url));
        }

        {
            // …unless we override it.
            let url = Url::parse("https://bar.matrix/").unwrap();
            let sync =
                client.sliding_sync("own-proxy")?.sliding_sync_proxy(url.clone()).build().await?;
            assert_eq!(sync.sliding_sync_proxy(), Some(url));
        }

        Ok(())
    }

    #[async_test]
    async fn test_limited_flag_computation() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let make_event = |event_id: &str| -> SyncTimelineEvent {
            SyncTimelineEvent::new(
                Raw::from_json_string(
                    json!({
                        "event_id": event_id,
                        "sender": "@johnmastodon:example.org",
                        "origin_server_ts": 1337424242,
                        "type": "m.room.message",
                        "room_id": "!meaningless:example.org",
                        "content": {
                            "body": "Hello, world!",
                            "msgtype": "m.text"
                        },
                    })
                    .to_string(),
                )
                .unwrap(),
            )
        };

        let event_a = make_event("$a");
        let event_b = make_event("$b");
        let event_c = make_event("$c");
        let event_d = make_event("$d");

        let not_initial = room_id!("!croissant:example.org");
        let no_overlap = room_id!("!omelette:example.org");
        let partial_overlap = room_id!("!fromage:example.org");
        let complete_overlap = room_id!("!baguette:example.org");
        let no_remote_events = room_id!("!pain:example.org");
        let no_local_events = room_id!("!crepe:example.org");
        let already_limited = room_id!("!paris:example.org");

        let response_timeline = vec![event_c.event.clone(), event_d.event.clone()];

        let local_rooms = BTreeMap::from_iter([
            (
                // This has no events overlapping with the response timeline, hence limited, but
                // it's not marked as initial in the response.
                not_initial.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    no_overlap.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_a.clone(), event_b.clone()],
                ),
            ),
            (
                // This has no events overlapping with the response timeline, hence limited.
                no_overlap.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    no_overlap.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_a.clone(), event_b.clone()],
                ),
            ),
            (
                // This has event_c in common with the response timeline.
                partial_overlap.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    partial_overlap.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_a.clone(), event_b.clone(), event_c.clone()],
                ),
            ),
            (
                // This has all events in common with the response timeline.
                complete_overlap.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    partial_overlap.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_c.clone(), event_d.clone()],
                ),
            ),
            (
                // We locally have events for this room, and receive none in the response: not
                // limited.
                no_remote_events.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    no_remote_events.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_c.clone(), event_d.clone()],
                ),
            ),
            (
                // We don't have events for this room locally, and even if the remote room contains
                // some events, it's not a limited sync.
                no_local_events.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    no_local_events.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![],
                ),
            ),
            (
                // Already limited, but would be marked limited if the flag wasn't ignored (same as
                // partial overlap).
                already_limited.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    already_limited.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_a.clone(), event_b.clone(), event_c.clone()],
                ),
            ),
        ]);

        let mut remote_rooms = BTreeMap::from_iter([
            (
                not_initial.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), { timeline: response_timeline }),
            ),
            (
                no_overlap.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![event_c.event.clone(), event_d.event.clone()],
                }),
            ),
            (
                partial_overlap.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![event_c.event.clone(), event_d.event.clone()],
                }),
            ),
            (
                complete_overlap.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![event_c.event.clone(), event_d.event.clone()],
                }),
            ),
            (
                no_remote_events.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![],
                }),
            ),
            (
                no_local_events.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![event_c.event.clone(), event_d.event.clone()],
                }),
            ),
            (
                already_limited.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    limited: true,
                    timeline: vec![event_c.event.clone(), event_d.event.clone()],
                }),
            ),
        ]);

        compute_limited(&local_rooms, &mut remote_rooms);

        assert!(!remote_rooms.get(not_initial).unwrap().limited);
        assert!(remote_rooms.get(no_overlap).unwrap().limited);
        assert!(!remote_rooms.get(partial_overlap).unwrap().limited);
        assert!(!remote_rooms.get(complete_overlap).unwrap().limited);
        assert!(!remote_rooms.get(no_remote_events).unwrap().limited);
        assert!(!remote_rooms.get(no_local_events).unwrap().limited);
        assert!(remote_rooms.get(already_limited).unwrap().limited);
    }

    #[async_test]
    #[cfg(feature = "e2e-encryption")]
    async fn test_process_only_encryption_events() -> Result<()> {
        let room = owned_room_id!("!croissant:example.org");

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let server_response = assign!(v4::Response::new("0".to_owned()), {
            rooms: BTreeMap::from([(
                room.clone(),
                assign!(v4::SlidingSyncRoom::default(), {
                    name: Some("Croissants lovers".to_owned()),
                    timeline: Vec::new(),
                }),
            )]),

            extensions: assign!(v4::Extensions::default(), {
                e2ee: assign!(v4::E2EE::default(), {
                    device_one_time_keys_count: BTreeMap::from([(DeviceKeyAlgorithm::SignedCurve25519, uint!(42))])
                }),
                to_device: Some(assign!(v4::ToDevice::default(), {
                    next_batch: "to-device-token".to_owned(),
                })),
            })
        });

        // Don't process non-encryption events if the sliding sync is configured for
        // encryption only.

        let sliding_sync = client
            .sliding_sync("test")?
            .with_to_device_extension(
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}))
            .build()
            .await?;

        {
            let mut position_guard = sliding_sync.inner.position.clone().lock_owned().await;

            sliding_sync.handle_response(server_response.clone(), &mut position_guard).await?;
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

            sliding_sync.handle_response(server_response.clone(), &mut position_guard).await?;
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
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}))
            .build()
            .await?;

        {
            let mut position_guard = sliding_sync.inner.position.clone().lock_owned().await;

            sliding_sync.handle_response(server_response.clone(), &mut position_guard).await?;
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
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}))
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

    #[cfg(not(target_arch = "wasm32"))] // b/o tokio::time::sleep
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
                            "sort": ["by_recency", "by_name"]
                        },
                        "another-list": {
                            "ranges": another_list_ranges,
                            "required_state": [
                                ["m.room.encryption", ""],
                                ["m.room.tombstone", ""]
                            ],
                            "sort": ["by_recency", "by_name"]
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
}
