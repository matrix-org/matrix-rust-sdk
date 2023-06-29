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

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    future::Future,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use async_stream::stream;
pub use builder::*;
pub use client::*;
pub use error::*;
use futures_core::stream::Stream;
pub use list::*;
pub use room::*;
use ruma::{
    api::client::{
        error::ErrorKind,
        sync::sync_events::v4::{self, ExtensionsConfig},
    },
    assign, OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select, spawn,
    sync::{broadcast::Sender, Mutex as AsyncMutex, RwLock as AsyncRwLock},
};
use tracing::{debug, error, instrument, warn, Instrument, Span};
use url::Url;

use self::{
    cache::restore_sliding_sync_state,
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

    /// A lock to ensure that responses are handled one at a time.
    response_handling_lock: Arc<AsyncMutex<()>>,
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
    polling_timeout: Duration,

    /// Extra duration for the sliding sync request to timeout. This is added to
    /// the [`Self::proxy_timeout`].
    network_timeout: Duration,

    /// The storage key to keep this cache at and load it from
    storage_key: String,

    /// Position markers
    position: StdRwLock<SlidingSyncPositionMarkers>,

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
        Self { inner: Arc::new(inner), response_handling_lock: Arc::new(AsyncMutex::new(())) }
    }

    async fn cache_to_storage(&self, to_device_token: Option<String>) -> Result<()> {
        cache::store_sliding_sync_state(self, to_device_token).await
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
        sliding_sync_response: v4::Response,
    ) -> Result<UpdateSummary, crate::Error> {
        // Transform a Sliding Sync Response to a `SyncResponse`.
        //
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `v4::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.
        let mut sync_response =
            self.inner.client.process_sliding_sync(&sliding_sync_response).await?;

        debug!(?sync_response, "Sliding Sync response has been handled by the client");

        {
            debug!(
                pos = ?sliding_sync_response.pos,
                delta_token = ?sliding_sync_response.delta_token,
                "Update position markers`"
            );

            let mut position_lock = self.inner.position.write().unwrap();
            position_lock.pos = Some(sliding_sync_response.pos);
            position_lock.delta_token = sliding_sync_response.delta_token;
        }

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

            UpdateSummary {
                lists: updated_lists,
                rooms: updated_rooms,
                to_device_token: sliding_sync_response
                    .extensions
                    .to_device
                    .map(|ext| ext.next_batch),
            }
        };

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
            let position_lock = self.inner.position.read().unwrap();

            (position_lock.pos.clone(), position_lock.delta_token.clone())
        };

        Span::current().record("pos", &pos);

        // Collect other data.
        let room_unsubscriptions = self.inner.room_unsubscriptions.read().unwrap().clone();

        let mut request = assign!(v4::Request::new(), {
            conn_id: Some(self.inner.id.clone()),
            pos,
            delta_token,
            timeout: Some(self.inner.polling_timeout),
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
            RequestConfig::default()
                .timeout(self.inner.polling_timeout + self.inner.network_timeout),
            room_unsubscriptions,
        ))
    }

    #[instrument(skip_all, fields(pos))]
    async fn sync_once(&self) -> Result<UpdateSummary> {
        let (request, request_config, requested_room_unsubscriptions) =
            self.generate_sync_request(&mut LazyTransactionId::new()).await?;

        debug!("Sending the sliding sync request");

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
            if self.inner.sticky.read().unwrap().data().extensions.e2ee.enabled == Some(true) {
                debug!("Sliding Sync is sending the request along with outgoing E2EE requests");

                let (e2ee_uploads, response) =
                    futures_util::future::join(self.inner.client.send_outgoing_requests(), request)
                        .await;

                if let Err(error) = e2ee_uploads {
                    error!(?error, "Error while sending outgoing E2EE requests");
                }

                response
            } else {
                debug!("Sliding Sync is sending the request (e2ee not enabled in this instance)");

                request.await
            }
        }?;

        // Send the request and get a response _without_ end-to-end encryption support.
        #[cfg(not(feature = "e2e-encryption"))]
        let response = {
            debug!("Sliding Sync is sending the request");

            request.await?
        };

        debug!("Sliding Sync response received");

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
            debug!("Sliding Sync response handling starts");

            // In case the task running this future is detached, we must
            // ensure responses are handled one at a time, hence we lock the
            // `response_handling_lock`.
            let response_handling_lock = this.response_handling_lock.lock().await;

            // Room unsubscriptions have been received by the server. We can update the
            // unsubscriptions buffer. However, it would be an error to empty it entirely as
            // more unsubscriptions could have been inserted during the request/response
            // dance. So let's cherry-pick which unsubscriptions to remove.
            {
                let room_unsubscriptions = &mut *this.inner.room_unsubscriptions.write().unwrap();

                room_unsubscriptions
                    .retain(|room_id| !requested_room_unsubscriptions.contains(room_id));
            }

            // Handle the response.
            let updates = this.handle_response(response).await?;

            this.cache_to_storage(updates.to_device_token.clone()).await?;

            // Release the lock.
            drop(response_handling_lock);

            debug!("Sliding Sync response has been fully handled");

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
    #[instrument(name = "sync_stream", skip_all)]
    pub fn sync(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        debug!(?self.inner.position, "About to run the sync-loop");

        let sync_span = Span::current();
        let mut internal_channel_receiver = self.inner.internal_channel.subscribe();

        stream! {
            loop {
                sync_span.in_scope(|| {
                    debug!(?self.inner.position, "Sync-loop is running");
                });

                select! {
                    biased;

                    internal_message = internal_channel_receiver.recv() => {
                        use SlidingSyncInternalMessage::*;

                        sync_span.in_scope(|| {
                            debug!(?internal_message, "Sync-loop has received an internal message");
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
                                        warn!("Session expired; resetting `pos` and sticky parameters");

                                        {
                                            let mut position_lock = self.inner.position.write().unwrap();
                                            position_lock.pos = None;
                                        }

                                        // Force invalidation of all the sticky parameters.
                                        let _ = self.inner.sticky.write().unwrap().data_mut();

                                        self.inner.lists.read().await.values().for_each(|list| list.invalidate_sticky_data());
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

            debug!("Sync-loop has exited.");
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

    /// Force caching the current sliding sync to storage.
    pub async fn force_cache_to_storage(&self, to_device_token: Option<String>) -> Result<()> {
        self.cache_to_storage(to_device_token).await
    }
}

#[derive(Debug)]
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

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    to_device_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_token: Option<String>,
}

impl FrozenSlidingSync {
    fn new(sliding_sync: &SlidingSync, to_device_since: Option<String>) -> Self {
        let position = sliding_sync.inner.position.read().unwrap();

        Self { delta_token: position.delta_token.clone(), to_device_since }
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
    /// The `prev_batch` token from the ToDevice extension, if any.
    pub to_device_token: Option<String>,
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
        assign!(request, {
            room_subscriptions: self.room_subscriptions.clone(),
            extensions: self.extensions.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use futures_util::{pin_mut, StreamExt};
    use matrix_sdk_test::async_test;
    use ruma::{api::client::sync::sync_events::v4::ToDeviceConfig, room_id, TransactionId};
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

        // When no to-device token is present, it's still not there after caching
        // either.
        let frozen = FrozenSlidingSync::new(&sliding_sync, None);
        assert!(frozen.to_device_since.is_none());

        // When a to-device token is present, `prepare_extensions_config` fills the
        // request with it.
        let since = String::from("my-to-device-since-token");

        let frozen = FrozenSlidingSync::new(&sliding_sync, Some(since.clone()));
        assert_eq!(frozen.to_device_since, Some(since));

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
        let since_token = "since";
        sync.force_cache_to_storage(Some(since_token.to_owned())).await?;

        let txn_id = TransactionId::new();
        let (request, _, _) = sync
            .generate_sync_request(&mut LazyTransactionId::from_owned(txn_id.to_owned()))
            .await?;

        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());
        assert_eq!(request.extensions.to_device.since.as_deref(), Some(since_token));

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

        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(|request: &Request| {
                // Repeat the txn_id in the response, if set.
                let request: PartialRequest = request.body_json().unwrap();
                ResponseTemplate::new(200).set_body_json(json!({
                    "txn_id": request.txn_id,
                    "pos": "0"
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let next = sync.next().await;
        assert_matches!(next, Some(Ok(_update_summary)));

        // `pos` has been updated.
        assert_eq!(sliding_sync.inner.position.read().unwrap().pos, Some("0".to_owned()));

        // Next request doesn't ask to enable the extension.
        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;
        assert!(request.extensions.to_device.enabled.is_none());

        let next = sync.next().await;
        assert_matches!(next, Some(Ok(_update_summary)));

        // Stop responding with successful requests!
        drop(_mock_guard);

        // When responding with M_UNKNOWN_POS, that regenerates the sticky parameters,
        // so they're reset. It also resets the `pos`.
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

        // Next request asks to enable the extension again.
        let (request, _, _) =
            sliding_sync.generate_sync_request(&mut LazyTransactionId::new()).await?;

        assert!(request.extensions.to_device.enabled.is_some());

        // `sync` has been stopped.
        assert!(sync.next().await.is_none());

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
}
