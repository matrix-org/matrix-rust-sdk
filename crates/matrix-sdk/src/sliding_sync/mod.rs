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
pub use builder::*;
pub use error::*;
use futures_core::stream::Stream;
pub use list::*;
use matrix_sdk_common::ring_buffer::RingBuffer;
pub use room::*;
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
    sync::{broadcast::Sender, Mutex as AsyncMutex, RwLock as AsyncRwLock},
};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};
use url::Url;
use utils::JoinHandleExt as _;

use self::{
    cache::restore_sliding_sync_state,
    sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager, StickyData},
};
use crate::{
    config::RequestConfig, sliding_sync::client::SlidingSyncResponseProcessor, Client, Result,
};

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

    /// The storage key to keep this cache at and load it from
    storage_key: String,

    /// Position markers
    position: StdRwLock<SlidingSyncPositionMarkers>,

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

    /// A lock to ensure that responses are handled one at a time.
    response_handling_lock: Arc<AsyncMutex<()>>,
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner) }
    }

    async fn cache_to_storage(&self) -> Result<()> {
        cache::store_sliding_sync_state(self).await
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

        // Compute `limited`.
        {
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
        if self.must_process_rooms_response().await {
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
        let mut position = self.inner.position.write().unwrap();
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
    ) -> Result<(v4::Request, RequestConfig, BTreeSet<OwnedRoomId>)> {
        // Collect requests for lists.
        let mut requests_lists = BTreeMap::new();

        {
            let lists = self.inner.lists.read().await;

            for (name, list) in lists.iter() {
                requests_lists.insert(name.clone(), list.next_request(txn_id)?);
            }
        }

        // Collect the `pos` and `delta_token`.
        let (pos, delta_token) = {
            let position = self.inner.position.read().unwrap();

            (position.pos.clone(), position.delta_token.clone())
        };

        Span::current().record("pos", &pos);

        // Collect other data.
        let room_unsubscriptions = self.inner.room_unsubscriptions.read().unwrap().clone();

        let mut request = assign!(v4::Request::new(), {
            conn_id: Some(self.inner.id.clone()),
            pos,
            delta_token,
            timeout: Some(self.inner.poll_timeout),
            lists: requests_lists,
            unsubscribe_rooms: room_unsubscriptions.iter().cloned().collect(),
        });

        let to_device_enabled = {
            let mut sticky_params = self.inner.sticky.write().unwrap();

            sticky_params.maybe_apply(&mut request, txn_id);

            sticky_params.data().extensions.to_device.enabled == Some(true)
        };

        // Set the to_device token if the extension is enabled.
        if to_device_enabled {
            let lists = self.inner.lists.read().await;

            let mut to_device_token = None;
            restore_sliding_sync_state(
                &self.inner.client,
                &self.inner.storage_key,
                &lists,
                &mut None,
                &mut to_device_token,
            )
            .await?;

            request.extensions.to_device.since = to_device_token;
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
        let (request, request_config, requested_room_unsubscriptions) =
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
            // ensure responses are handled one at a time, hence we lock the
            // `response_handling_lock`.
            let response_handling_lock = this.inner.response_handling_lock.lock().await;

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
            let updates = this.handle_response(response).await?;

            this.cache_to_storage().await?;

            // Release the lock.
            drop(response_handling_lock);

            debug!("Done handling response");

            Ok(updates)
        };

        spawn(future.instrument(Span::current())).await.unwrap()
    }

    /// Create a _new_ Sliding Sync sync-loop.
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

    /// Force to stop the sync-loop ([`Self::sync`]) if it's running.
    ///
    /// Usually, dropping the `Stream` returned by [`Self::sync`] should be
    /// enough to “stop” it, but depending of how this `Stream` is used, it
    /// might not be obvious to drop it immediately (thinking of using this API
    /// over FFI; the foreign-language might not be able to drop a value
    /// immediately). Thus, calling this method will ensure that the sync-loop
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
    /// This method **MUST** be called when the sync-loop is stopped.
    #[doc(hidden)]
    pub async fn expire_session(&self) {
        info!("Session expired; resetting `pos` and sticky parameters");

        {
            let mut position = self.inner.position.write().unwrap();
            position.pos = None;

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
    /// the sync-loop is running.
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
        let position_lock = self.inner.position.read().unwrap();
        position_lock.pos.clone()
    }

    /// Set a new value for `pos`.
    pub fn set_pos(&self, new_pos: String) {
        let mut position_lock = self.inner.position.write().unwrap();
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
    fn new(sliding_sync: &SlidingSync) -> Self {
        let position = sliding_sync.inner.position.read().unwrap();

        // The to-device token must be saved in the `FrozenCryptoSlidingSync` now.
        Self { delta_token: position.delta_token.clone(), to_device_since: None }
    }
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
#[instrument(skip_all)]
fn compute_limited(
    known_rooms: &BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    response_rooms: &mut BTreeMap<OwnedRoomId, v4::SlidingSyncRoom>,
) {
    for (room_id, room) in response_rooms {
        // Only rooms marked as initially loaded are subject to the fixup.
        let initial = room.initial.unwrap_or(false);
        if !initial {
            continue;
        }

        if room.limited {
            // If the room was already marked as limited, the server knew more than we do.
            continue;
        }

        trace!(?room_id, "starting");

        // If the known room had some timeline events, consider it's a `limited` if
        // there's absolutely no overlap between the known events and
        // the new events in the timeline.
        if let Some(known_room) = known_rooms.get(room_id) {
            let known_events = known_room.timeline_queue();

            if room.timeline.is_empty() && !known_events.is_empty() {
                // If the cached timeline had events, but the one in the response didn't have
                // any, don't mark the room as limited.
                trace!(
                    "no timeline updates in response, local timeline had events => not limited."
                );
                continue;
            }

            // Gather all the known event IDs. Ignore events that don't have an event ID.
            let num_known_events = known_events.len();
            let known_events: HashSet<OwnedEventId> =
                HashSet::from_iter(known_events.into_iter().filter_map(|event| event.event_id()));

            if num_known_events != known_events.len() {
                trace!(
                    "{} local timeline events had no IDs",
                    num_known_events - known_events.len()
                );
            }

            // There's overlap if, and only if, there's at least one event in the
            // response's timeline that matches an event id we've seen before.
            let mut num_missing_event_ids = 0;
            let overlap = room.timeline.iter().any(|seen_event| {
                if let Some(seen_event_id) =
                    seen_event.get_field::<OwnedEventId>("event_id").ok().flatten()
                {
                    known_events.contains(&seen_event_id)
                } else {
                    warn!("unable to get event_id from {seen_event:?} when computing limited flag");
                    num_missing_event_ids += 1;
                    false
                }
            });

            room.limited = !overlap;

            trace!(
                num_events_response = room.timeline.len(),
                num_events_local = known_events.len(),
                num_missing_event_ids,
                room_limited = room.limited,
                "done"
            );
        } else {
            trace!("room isn't known locally => not limited");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use futures_util::{pin_mut, StreamExt};
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::sync::sync_events::v4::ToDeviceConfig, owned_room_id, room_id, serde::Raw,
        uint, DeviceKeyAlgorithm, TransactionId,
    };
    use serde_json::json;
    use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::{
        sliding_sync::sticky_parameters::SlidingSyncStickyManager, test_utils::logged_in_client,
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
        let frozen = FrozenSlidingSync::new(&sliding_sync);
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
        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
        assert!(request.extensions.to_device.enabled.is_some());

        let sync = sliding_sync.sync();
        pin_mut!(sync);

        // `pos` is `None` to start with.
        assert!(sliding_sync.inner.position.read().unwrap().pos.is_none());

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
            assert_eq!(sliding_sync.inner.position.read().unwrap().pos, Some("0".to_owned()));

            // `past_positions` has been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 1);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
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
            assert_eq!(sliding_sync.inner.position.read().unwrap().pos, Some("1".to_owned()));

            // `past_positions` has been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 2);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
            assert_eq!(past_positions.get(1).unwrap().pos, Some("1".to_owned()));
        }

        // Next request isn't successful because it receives an already
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
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(
                next,
                Some(Err(crate::Error::SlidingSync(Error::ResponseAlreadyReceived { pos }))) => {
                    assert_eq!(pos, Some("0".to_owned()));
                }
            );

            // `sync` has been stopped.
            assert!(sync.next().await.is_none());

            // `pos` has not been updated.
            assert_eq!(sliding_sync.inner.position.read().unwrap().pos, Some("1".to_owned()));

            // `past_positions` has not been updated.
            let past_positions = sliding_sync.inner.past_positions.read().unwrap();
            assert_eq!(past_positions.len(), 2);
            assert_eq!(past_positions.get(0).unwrap().pos, Some("0".to_owned()));
            assert_eq!(past_positions.get(1).unwrap().pos, Some("1".to_owned()));
        }

        // Restart the sync.
        let sync = sliding_sync.sync();
        pin_mut!(sync);

        // Next request is successful.
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(|request: &Request| {
                    // Repeat the txn_id in the response, if set.
                    let request: PartialRequest = request.body_json().unwrap();

                    ResponseTemplate::new(200).set_body_json(json!({
                        "txn_id": request.txn_id,
                        "pos": "2",
                    }))
                })
                .mount_as_scoped(&server)
                .await;

            let next = sync.next().await;
            assert_matches!(next, Some(Ok(_update_summary)));

            // `pos` has been updated.
            assert_eq!(sliding_sync.inner.position.read().unwrap().pos, Some("2".to_owned()));

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
            assert!(sliding_sync.inner.position.read().unwrap().pos.is_none());

            // `past_positions` has been reset.
            assert!(sliding_sync.inner.past_positions.read().unwrap().is_empty());

            // Next request asks to enable the extension again.
            let (request, _, _) =
                sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

            assert!(request.extensions.to_device.enabled.is_some());

            // `sync` has been stopped.
            assert!(sync.next().await.is_none());
        }

        Ok(())
    }

    #[async_test]
    async fn test_stop_sync_loop() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        // Start the sync-loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync-loop is actually running.
        assert!(stream.next().await.is_some());

        // Stop the sync-loop.
        sliding_sync.stop_sync()?;

        // The sync-loop is actually stopped.
        assert!(stream.next().await.is_none());

        // Start a new sync-loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync-loop is actually running.
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
        let no_new_events = room_id!("!pain:example.org");
        let already_limited = room_id!("!paris:example.org");

        let response_timeline = vec![event_c.event.clone(), event_d.event.clone()];

        let known_rooms = BTreeMap::from_iter([
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
                no_new_events.to_owned(),
                SlidingSyncRoom::new(
                    client.clone(),
                    no_new_events.to_owned(),
                    v4::SlidingSyncRoom::default(),
                    vec![event_c.clone(), event_d.clone()],
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

        let mut response_rooms = BTreeMap::from_iter([
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
                no_new_events.to_owned(),
                assign!(v4::SlidingSyncRoom::default(), {
                    initial: Some(true),
                    timeline: vec![],
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

        compute_limited(&known_rooms, &mut response_rooms);

        assert!(!response_rooms.get(not_initial).unwrap().limited);
        assert!(response_rooms.get(no_overlap).unwrap().limited);
        assert!(!response_rooms.get(partial_overlap).unwrap().limited);
        assert!(!response_rooms.get(complete_overlap).unwrap().limited);
        assert!(!response_rooms.get(no_new_events).unwrap().limited);
        assert!(response_rooms.get(already_limited).unwrap().limited);
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

        sliding_sync.handle_response(server_response.clone()).await?;

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

        sliding_sync.handle_response(server_response.clone()).await?;

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

        sliding_sync.handle_response(server_response.clone()).await?;

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
}
