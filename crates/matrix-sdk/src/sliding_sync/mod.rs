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

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, RwLock as StdRwLock,
    },
    time::Duration,
};

use async_stream::stream;
pub use builder::*;
pub use client::*;
pub use error::*;
use eyeball::unique::Observable;
use futures_core::stream::Stream;
pub use list::*;
pub use room::*;
use ruma::{
    api::client::{
        error::ErrorKind,
        sync::sync_events::v4::{self, ExtensionsConfig},
    },
    assign,
    events::TimelineEventType,
    OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select, spawn,
    sync::{
        mpsc::{Receiver, Sender},
        Mutex as AsyncMutex, RwLock as AsyncRwLock,
    },
};
use tracing::{debug, error, instrument, warn, Instrument, Span};
use url::Url;

use crate::{config::RequestConfig, Client, Result};

/// Number of times a Sliding Sync session can expire before raising an error.
///
/// A Sliding Sync session can expire. In this case, it is reset. However, to
/// avoid entering an infinite loop of “it's expired, let's reset, it's expired,
/// let's reset…” (maybe if the network has an issue, or the server, or anything
/// else), we define a maximum times a session can expire before
/// raising a proper error.
const MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION: u8 = 3;

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

mod sticky_parameters {
    //! Sticky parameters are implemented in the context of a different mod, so
    //! it's not possible write to the sticky parameters fields without
    //! invalidating the sticky parameters.
    //!
    //! At the moment, the granularity of stickiness is the entire set of sticky
    //! parameters; if needs be, we'll change that in the future to have
    //! something that is per sticky field.
    use ruma::{OwnedTransactionId, TransactionId};

    use super::*;

    #[derive(Debug)]
    pub struct StickyParameters {
        /// Was any of the parameters invalidated? If yes, reinitialize them.
        invalidated: bool,

        /// If the sticky parameters were applied to a given request, this is
        /// the transaction id generated for that request, that must be matched
        /// upon in the next call to `commit()`.
        txn_id: Option<OwnedTransactionId>,

        /// Room subscriptions, i.e. rooms that may be out-of-scope of all lists
        /// but one wants to receive updates.
        room_subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,

        /// The `bump_event_types` field. See
        /// [`SlidingSyncBuilder::bump_event_types`] to learn more.
        bump_event_types: Vec<TimelineEventType>,

        /// The intended state of the extensions being supplied to sliding /sync
        /// calls.
        extensions: ExtensionsConfig,
    }

    impl StickyParameters {
        /// Create a new set of sticky parameters.
        pub fn new(
            bump_event_types: Vec<TimelineEventType>,
            room_subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
            mut extensions: ExtensionsConfig,
        ) -> Self {
            // Strip the to-device since token from the extensions configuration, in case it was set; it should only be set later.
            // TODO can remove after https://github.com/matrix-org/matrix-rust-sdk/pull/1963.
            extensions.to_device.since = None;

            // Only initially mark as invalidated if there's any meaningful data.
            let invalidated = !room_subscriptions.is_empty()
                || !bump_event_types.is_empty()
                || extensions != ExtensionsConfig::default();

            Self { invalidated, txn_id: None, room_subscriptions, bump_event_types, extensions }
        }

        /// Marks the sticky parameters as acknowledged by the server.
        pub fn maybe_commit(&mut self, response_txn_id: &TransactionId) {
            if self.invalidated {
                // Don't make it a hard error if the response transaction id doesn't match the
                // expected one; this might be caused by an internal reset, or
                // because the server returned an unknown position, which also
                // "resets" the current sliding sync.
                if self.txn_id.as_deref() == Some(response_txn_id) {
                    self.invalidated = false;
                }
            }
        }

        /// Manually invalidate all the sticky parameters.
        pub fn invalidate(&mut self) {
            self.invalidated = true;
            self.txn_id = None;
        }

        /// May apply some sticky parameters.
        ///
        /// After receiving the response from this sliding sync, the caller MUST
        /// also call [`Self::commit`] with the transaction id from the server's
        /// response.
        pub fn maybe_apply_parameters(&mut self, request: &mut v4::Request) {
            if !self.invalidated {
                return;
            }
            let tid = TransactionId::new();
            assign!(request, {
                txn_id: Some(tid.to_string()),
                room_subscriptions: self.room_subscriptions.clone(),
                bump_event_types: self.bump_event_types.clone(),
                extensions: self.extensions.clone(),
            });
            self.txn_id = Some(tid);
        }

        /// Gets mutable access to the room subscriptions.
        ///
        /// Assuming it will change something, invalidates the parameters.
        pub fn room_subscriptions_mut(
            &mut self,
        ) -> &mut BTreeMap<OwnedRoomId, v4::RoomSubscription> {
            self.invalidated = true;
            &mut self.room_subscriptions
        }

        /// Extensions configured for this client.
        pub fn extensions(&self) -> &ExtensionsConfig {
            &self.extensions
        }

        /// Gets read-only access to the room subscriptions.
        ///
        /// Doesn't invalidate the room parameters.
        #[cfg(test)]
        pub fn room_subscriptions(&self) -> &BTreeMap<OwnedRoomId, v4::RoomSubscription> {
            &self.room_subscriptions
        }

        #[cfg(test)]
        pub fn is_invalidated(&self) -> bool {
            self.invalidated
        }
    }
}

#[derive(Debug)]
pub(super) struct SlidingSyncInner {
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    /// The HTTP Matrix client.
    client: Client,

    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    /// The `pos`  and `delta_token` markers.
    position: StdRwLock<SlidingSyncPositionMarkers>,

    /// The lists of this Sliding Sync instance.
    lists: StdRwLock<BTreeMap<String, SlidingSyncList>>,

    /// The rooms details
    rooms: StdRwLock<BTreeMap<OwnedRoomId, SlidingSyncRoom>>,

    /// Request parameters that are sticky.
    sticky: StdRwLock<sticky_parameters::StickyParameters>,

    /// Rooms to unsubscribe, see [`Self::room_subscriptions`].
    room_unsubscriptions: StdRwLock<BTreeSet<OwnedRoomId>>,

    /// Number of times a Sliding Sync session has been reset.
    reset_counter: AtomicU8,

    /// The intended state of the extensions being supplied to sliding /sync
    /// calls. May contain the latest next_batch for to_devices, etc.
    extensions: Mutex<Option<ExtensionsConfig>>,

    /// Internal channel used to pass messages between Sliding Sync and other
    /// types.
    internal_channel:
        (Sender<SlidingSyncInternalMessage>, AsyncRwLock<Receiver<SlidingSyncInternalMessage>>),
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner), response_handling_lock: Arc::new(AsyncMutex::new(())) }
    }

    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        cache::store_sliding_sync_state(self).await
    }

    /// Create a new [`SlidingSyncBuilder`].
    pub fn builder(client: Client) -> SlidingSyncBuilder {
        SlidingSyncBuilder::new(client)
    }

    /// Subscribe to a given room.
    pub async fn subscribe_to_room(
        &self,
        room_id: OwnedRoomId,
        settings: Option<v4::RoomSubscription>,
    ) -> Result<()> {
        self.inner
            .sticky
            .write()
            .unwrap()
            .room_subscriptions_mut()
            .insert(room_id, settings.unwrap_or_default());
        self.inner.internal_channel_send(SlidingSyncInternalMessage::ContinueSyncLoop).await?;

        Ok(())
    }

    /// Unsubscribe from a given room.
    pub async fn unsubscribe_from_room(&self, room_id: OwnedRoomId) -> Result<()> {
        // If removing the subscription was successful…
        if self.inner.sticky.write().unwrap().room_subscriptions_mut().remove(&room_id).is_some() {
            // … then keep the unsubscription for the next request.
            self.inner.room_unsubscriptions.write().unwrap().insert(room_id);
            self.inner.internal_channel_send(SlidingSyncInternalMessage::ContinueSyncLoop).await?;
        }

        Ok(())
    }

    /// Add the common extensions if not already configured.
    pub fn add_common_extensions(&self) {
        let mut lock = self.inner.extensions.lock().unwrap();
        let cfg = lock.get_or_insert_with(Default::default);

        if cfg.to_device.enabled.is_none() {
            cfg.to_device.enabled = Some(true);
        }

        if cfg.e2ee.enabled.is_none() {
            cfg.e2ee.enabled = Some(true);
        }

        if cfg.account_data.enabled.is_none() {
            cfg.account_data.enabled = Some(false);
        }
    }

    /// Lookup a specific room
    pub fn get_room(&self, room_id: &RoomId) -> Option<SlidingSyncRoom> {
        self.inner.rooms.read().unwrap().get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.inner.rooms.read().unwrap().len()
    }

    #[instrument(skip(self))]
    fn update_to_device_since(&self, since: String) {
        // FIXME: Find a better place where the to-device since token should be
        // persisted.
        self.inner
            .extensions
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .to_device
            .since = Some(since);
    }

    /// Find a list by its name, and do something on it if it exists.
    pub fn on_list<F, R>(&self, list_name: &str, f: F) -> Option<R>
    where
        F: FnOnce(&SlidingSyncList) -> R,
    {
        let lists = self.inner.lists.read().unwrap();

        lists.get(list_name).map(f)
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
        let list = list_builder.build(self.inner.internal_channel.0.clone());
        self.inner.internal_channel_send(SlidingSyncInternalMessage::ContinueSyncLoop).await?;

        Ok(self.inner.lists.write().unwrap().insert(list.name().to_owned(), list))
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
        let Some(ref storage_key) = self.inner.storage_key else {
            return Err(error::Error::MissingStorageKeyForCaching.into());
        };

        let reloaded_rooms =
            list_builder.set_cached_and_reload(&self.inner.client, storage_key).await?;

        if !reloaded_rooms.is_empty() {
            let mut rooms = self.inner.rooms.write().unwrap();

            for (key, frozen) in reloaded_rooms {
                rooms.entry(key).or_insert_with(|| {
                    SlidingSyncRoom::from_frozen(frozen, self.inner.client.clone())
                });
            }
        }

        self.add_list(list_builder).await
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.inner.rooms.read().unwrap();

        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.inner.rooms.read().unwrap().values().cloned().collect()
    }

    /// Handle the HTTP response.
    #[instrument(skip_all, fields(lists = self.inner.lists.read().unwrap().len()))]
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
            Observable::set(&mut position_lock.pos, Some(sliding_sync_response.pos));
            Observable::set(&mut position_lock.delta_token, sliding_sync_response.delta_token);
        }

        // Commit sticky parameters, if needed.
        if let Some(ref txn_id) = sliding_sync_response.txn_id {
            self.inner.sticky.write().unwrap().maybe_commit(txn_id.as_str().into());
        }

        let update_summary = {
            // Update the rooms.
            let updated_rooms = {
                let mut rooms_map = self.inner.rooms.write().unwrap();

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
                let mut updated_lists = Vec::with_capacity(sliding_sync_response.lists.len());
                let mut lists = self.inner.lists.write().unwrap();

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

                // Update the `to-device` next-batch if any.
                if let Some(to_device) = sliding_sync_response.extensions.to_device {
                    self.update_to_device_since(to_device.next_batch);
                }

                updated_lists
            };

            UpdateSummary { lists: updated_lists, rooms: updated_rooms }
        };

        Ok(update_summary)
    }

    fn generate_sync_request(
        &self,
    ) -> Result<Option<(v4::Request, RequestConfig, BTreeSet<OwnedRoomId>)>> {
        // Collect requests for lists.
        let mut requests_lists = BTreeMap::new();

        {
            let mut lists = self.inner.lists.write().unwrap();

            if lists.is_empty() {
                return Ok(None);
            }

            for (name, list) in lists.iter_mut() {
                requests_lists.insert(name.clone(), list.next_request()?);
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
        let timeout = Duration::from_secs(30);

        let mut request = assign!(v4::Request::new(), {
            pos,
            delta_token,
            timeout: Some(timeout),
            lists: requests_lists,
            unsubscribe_rooms: room_unsubscriptions.iter().cloned().collect(),
        });

        {
            let mut sticky_params = self.inner.sticky.write().unwrap();
            sticky_params.maybe_apply_parameters(&mut request);

            // Set the to_device token if the extension is enabled.
            if sticky_params.extensions().to_device.enabled == Some(true) {
                // TODO once https://github.com/matrix-org/matrix-rust-sdk/pull/1963 lands, gets rid of inner.extensions here.
                let extensions = self.inner.extensions.lock().unwrap().clone().unwrap_or_default();
                request.extensions.to_device.since = extensions.to_device.since;
            }
        }

        Ok(Some((
            // Build the request itself.
            request,
            // Configure long-polling. We need 30 seconds for the long-poll itself, in
            // addition to 30 more extra seconds for the network delays.
            RequestConfig::default().timeout(timeout + Duration::from_secs(30)),
            room_unsubscriptions,
        )))
    }

    #[instrument(skip_all, fields(pos))]
    async fn sync_once(&self) -> Result<Option<UpdateSummary>> {
        let (request, request_config, requested_room_unsubscriptions) =
            match self.generate_sync_request()? {
                Some(v) => v,
                None => return Ok(None),
            };

        debug!("Sending the sliding sync request");

        // Prepare the request.
        let request = self.inner.client.send_with_homeserver(
            request,
            Some(request_config),
            self.inner.homeserver.as_ref().map(ToString::to_string),
        );

        // Send the request and get a response with end-to-end encryption support.
        //
        // Sending the `/sync` request out when end-to-end encryption is enabled means
        // that we need to also send out any outgoing e2ee related request out
        // coming from the `OlmMachine::outgoing_requests()` method.
        #[cfg(feature = "e2e-encryption")]
        let response = {
            debug!("Sliding Sync is sending the request along with  outgoing E2EE requests");

            let (e2ee_uploads, response) =
                futures_util::future::join(self.inner.client.send_outgoing_requests(), request)
                    .await;

            if let Err(error) = e2ee_uploads {
                error!(?error, "Error while sending outgoing E2EE requests");
            }

            response
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

            this.cache_to_storage().await?;

            // Release the lock.
            drop(response_handling_lock);

            debug!("Sliding Sync response has been fully handled");

            Ok(Some(updates))
        };

        spawn(future.instrument(Span::current())).await.unwrap()
    }

    /// Create a _new_ Sliding Sync stream.
    ///
    /// This stream will send requests and will handle responses automatically,
    /// hence updating the lists.
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(name = "sync_stream", skip_all)]
    pub fn stream(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        debug!(?self.inner.extensions, "About to run the sync stream");

        let sync_stream_span = Span::current();

        stream! {
            loop {
                sync_stream_span.in_scope(|| {
                    debug!(?self.inner.extensions, "Sync stream loop is running");
                });

                let mut internal_channel_receiver_lock = self.inner.internal_channel.1.write().await;

                select! {
                    biased;

                    internal_message = internal_channel_receiver_lock.recv() => {
                        use SlidingSyncInternalMessage::*;

                        match internal_message {
                            None | Some(BreakSyncLoop) => {
                                break;
                            }

                            Some(ContinueSyncLoop) => {
                                continue;
                            }
                        }
                    }

                    update_summary = self.sync_once().instrument(sync_stream_span.clone()) => {
                        match update_summary {
                            Ok(Some(updates)) => {
                                self.inner.reset_counter.store(0, Ordering::SeqCst);

                                yield Ok(updates);
                            }

                            Ok(None) => {
                                break;
                            }

                            Err(error) => {
                                if error.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                                    // The session has expired.

                                    // Has it expired too many times?
                                    if self.inner.reset_counter.fetch_add(1, Ordering::SeqCst)
                                        >= MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION
                                    {
                                        sync_stream_span.in_scope(|| {
                                            error!("Session expired {MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION} times in a row");
                                        });

                                        // The session has expired too many times, let's raise an error!
                                        yield Err(error);

                                        break;
                                    }

                                    // Let's reset the Sliding Sync session.
                                    sync_stream_span.in_scope(|| {
                                        warn!("Session expired. Restarting Sliding Sync.");

                                        // To “restart” a Sliding Sync session, we set `pos` to its initial value, and uncommit the sticky parameters, so they're sent next time.
                                        {
                                            let mut position_lock = self.inner.position.write().unwrap();

                                            Observable::set(&mut position_lock.pos, None);
                                        }

                                        self.inner.sticky.write().unwrap().invalidate();

                                        debug!(?self.inner.extensions, "Sliding Sync has been reset");
                                    });
                                }

                                yield Err(error);

                                continue;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Resets the lists.
    pub fn reset_lists(&self) -> Result<(), Error> {
        let lists = self.inner.lists.read().unwrap();

        for list in lists.values() {
            list.reset()?;
        }

        Ok(())
    }
}

impl SlidingSyncInner {
    /// Send a message over the internal channel.
    async fn internal_channel_send(&self, message: SlidingSyncInternalMessage) -> Result<()> {
        Ok(self
            .internal_channel
            .0
            .send(message)
            .await
            .map_err(|_| Error::InternalChannelIsBroken)?)
    }
}

#[derive(Debug)]
enum SlidingSyncInternalMessage {
    #[allow(unused)] // temporary
    BreakSyncLoop,
    ContinueSyncLoop,
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

        Observable::set(&mut position_lock.pos, Some(new_pos));
    }
}

#[derive(Debug)]
pub(super) struct SlidingSyncPositionMarkers {
    /// An ephemeral position in the current stream, as received from the
    /// previous /sync response, or `None` for the first request.
    ///
    /// Should not be persisted.
    pos: Observable<Option<String>>,

    /// Server-provided opaque token that remembers what the last timeline and
    /// state events stored by the client were.
    ///
    /// If `None`, the server will send the
    /// full information for all the lists present in the request.
    delta_token: Observable<Option<String>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    to_device_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_token: Option<String>,
}

impl From<&SlidingSync> for FrozenSlidingSync {
    fn from(sliding_sync: &SlidingSync) -> Self {
        FrozenSlidingSync {
            delta_token: sliding_sync.inner.position.read().unwrap().delta_token.clone(),
            to_device_since: sliding_sync
                .inner
                .extensions
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|ext| ext.to_device.since.clone()),
        }
    }
}

/// A summary of the updates received after a sync (like in
/// [`SlidingSync::stream`]).
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The names of the lists that have seen an update.
    pub lists: Vec<String>,
    /// The rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

#[cfg(test)]
mod test {
    use futures_util::pin_mut;
    use ruma::room_id;
    use wiremock::MockServer;

    use super::{sticky_parameters::StickyParameters, *};
    use crate::test_utils::logged_in_client;

    async fn new_sliding_sync(
        lists: Vec<SlidingSyncListBuilder>,
    ) -> Result<(MockServer, SlidingSync)> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut sliding_sync_builder = client.sliding_sync().await;

        for list in lists {
            sliding_sync_builder = sliding_sync_builder.add_list(list);
        }

        let sliding_sync = sliding_sync_builder.build().await?;

        Ok((server, sliding_sync))
    }

    #[tokio::test]
    async fn test_subscribe_to_room() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .set_range(0..=10)])
        .await?;

        let _stream = sliding_sync.stream();
        pin_mut!(_stream);

        let room0 = room_id!("!r0:bar.org").to_owned();
        let room1 = room_id!("!r1:bar.org").to_owned();
        let room2 = room_id!("!r2:bar.org").to_owned();

        sliding_sync.subscribe_to_room(room0.clone(), None).await?;
        sliding_sync.subscribe_to_room(room1.clone(), None).await?;

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = sticky.room_subscriptions();

            assert!(room_subscriptions.contains_key(&room0));
            assert!(room_subscriptions.contains_key(&room1));
            assert!(!room_subscriptions.contains_key(&room2));
        }

        sliding_sync.unsubscribe_from_room(room0.clone()).await?;
        sliding_sync.unsubscribe_from_room(room2.clone()).await?;

        {
            let sticky = sliding_sync.inner.sticky.read().unwrap();
            let room_subscriptions = sticky.room_subscriptions();

            assert!(!room_subscriptions.contains_key(&room0));
            assert!(room_subscriptions.contains_key(&room1));
            assert!(!room_subscriptions.contains_key(&room2));

            let room_unsubscriptions = sliding_sync.inner.room_unsubscriptions.read().unwrap();

            assert!(room_unsubscriptions.contains(&room0));
            assert!(!room_unsubscriptions.contains(&room1));
            assert!(!room_unsubscriptions.contains(&room2));
        }

        // this test also ensures that Tokio is not panicking when calling
        // `subscribe_to_room` and `unsubscribe_from_room`.

        Ok(())
    }

    #[tokio::test]
    async fn test_add_list() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .set_range(0..=10)])
        .await?;

        let _stream = sliding_sync.stream();
        pin_mut!(_stream);

        sliding_sync
            .add_list(
                SlidingSyncList::builder("bar")
                    .sync_mode(SlidingSyncMode::Selective)
                    .set_range(50..=60),
            )
            .await?;

        let lists = sliding_sync.inner.lists.read().unwrap();

        assert!(lists.contains_key("foo"));
        assert!(lists.contains_key("bar"));

        // this test also ensures that Tokio is not panicking when calling `add_list`.

        Ok(())
    }

    #[test]
    fn test_sticky_parameters_api_non_invalidated_no_effect() {
        let mut sticky =
            StickyParameters::new(Default::default(), Default::default(), Default::default());
        assert!(!sticky.is_invalidated(), "not invalidated if constructed with default parameters");

        let mut request = v4::Request::default();
        sticky.maybe_apply_parameters(&mut request);

        assert_eq!(request.txn_id, None);
        assert_eq!(request.room_subscriptions.len(), 0);

        assert!(!sticky.is_invalidated());

        // Committing while it wasn't expected isn't a hard error, and it doesn't change
        // anything.
        sticky.maybe_commit("tid123".into());

        assert!(!sticky.is_invalidated());
    }

    #[test]
    fn test_sticky_parameters_api_invalidated_flow() {
        // Construct a valid, non-empty room subscriptions list.
        let mut room_subs: BTreeMap<OwnedRoomId, v4::RoomSubscription> = Default::default();
        let r0 = room_id!("!r0:bar.org").to_owned();
        room_subs.insert(r0.clone(), Default::default());

        // At first it's invalidated.
        let mut sticky = StickyParameters::new(Default::default(), room_subs, Default::default());
        assert!(sticky.is_invalidated(), "invalidated because of non default parameters");

        // Then when we create a request, the sticky parameters are applied.
        let mut request = v4::Request::default();
        sticky.maybe_apply_parameters(&mut request);

        assert!(request.txn_id.is_some());
        assert_eq!(request.room_subscriptions.len(), 1);
        assert!(request.room_subscriptions.get(&r0).is_some());

        let tid = request.txn_id.expect("invalidated + apply_parameters => non-null tid");

        sticky.maybe_commit(tid.as_str().into());
        assert!(!sticky.is_invalidated());

        // Applying new parameters will invalidate again.
        sticky
            .room_subscriptions_mut()
            .insert(room_id!("!r1:bar.org").to_owned(), Default::default());
        assert!(sticky.is_invalidated());

        // Committing with the wrong transaction id will keep it invalidated.
        sticky.maybe_commit("what-are-the-odds-the-rng-will-generate-this-string".into());
        assert!(sticky.is_invalidated());

        // Restarting a request will only remember the last generated transaction id.
        let mut request1 = v4::Request::default();
        sticky.maybe_apply_parameters(&mut request1);
        assert!(sticky.is_invalidated());
        assert!(request1.txn_id.is_some());
        assert_eq!(request1.room_subscriptions.len(), 2);

        let mut request2 = v4::Request::default();
        sticky.maybe_apply_parameters(&mut request2);
        assert!(sticky.is_invalidated());
        assert!(request2.txn_id.is_some());
        assert_eq!(request2.room_subscriptions.len(), 2);

        // Here we commit with the not most-recent TID, so it keeps the invalidated
        // status.
        sticky.maybe_commit(request1.txn_id.unwrap().as_str().into());
        assert!(sticky.is_invalidated());

        // But here we use the latest TID, so the commit is effective.
        sticky.maybe_commit(request2.txn_id.unwrap().as_str().into());
        assert!(!sticky.is_invalidated());
    }

    #[test]
    fn test_extensions_are_sticky() {
        let mut extensions = ExtensionsConfig::default();
        extensions.account_data.enabled = Some(true);
        extensions.to_device.since = Some("since".to_owned());

        // At first it's invalidated.
        let mut sticky = StickyParameters::new(Default::default(), Default::default(), extensions);

        assert!(sticky.is_invalidated(), "invalidated because of non default parameters");

        // `StickyParameters::new` follows its caller's intent when it comes to e2ee and to-device.
        assert_eq!(sticky.extensions().e2ee.enabled, None);
        assert_eq!(sticky.extensions().to_device.enabled, None,);
        assert_eq!(sticky.extensions().to_device.since, None,);

        // What the user explicitly enabled is... enabled.
        assert_eq!(sticky.extensions().account_data.enabled, Some(true),);

        let mut request = v4::Request::default();
        sticky.maybe_apply_parameters(&mut request);
        assert!(sticky.is_invalidated());
        assert!(request.txn_id.is_some());
        assert_eq!(request.extensions.to_device.enabled, None);
        assert_eq!(request.extensions.to_device.since, None);
        assert_eq!(request.extensions.e2ee.enabled, None);
        assert_eq!(request.extensions.account_data.enabled, Some(true));
    }

    #[tokio::test]
    async fn test_sticky_extensions_plus_since() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ss_builder = client.sliding_sync().await;
        ss_builder = ss_builder.add_list(SlidingSyncList::builder("new_list"));

        let sync = ss_builder.build().await?;

        // We get to-device and e2ee even without requesting it.
        assert_eq!(sync.inner.sticky.read().unwrap().extensions().to_device.enabled, Some(true));
        assert_eq!(sync.inner.sticky.read().unwrap().extensions().e2ee.enabled, Some(true));
        // But what we didn't enable... isn't enabled.
        assert_eq!(sync.inner.sticky.read().unwrap().extensions().account_data.enabled, None);

        // Even without a since token, the first request will contain the extensions configuration, at least.
        let (request, _, _) = sync
            .generate_sync_request()?
            .expect("must have generated a request because there's one list");

        let txn_id = request.txn_id.unwrap();
        assert_eq!(request.extensions.e2ee.enabled, Some(true));
        assert_eq!(request.extensions.to_device.enabled, Some(true));
        assert!(request.extensions.to_device.since.is_none());

        {
            // Committing with another transaction id doesn't validate anything.
            let mut sticky = sync.inner.sticky.write().unwrap();
            assert!(sticky.is_invalidated());
            sticky.maybe_commit("very-hopeful-no-rng-will-generate-this-string".into());
            assert!(sticky.is_invalidated());
        }

        // Regenerating a request will yield the same one.
        let (request, _, _) = sync
            .generate_sync_request()?
            .expect("must have generated a request because there's one list");

        let txn_id2 = request.txn_id.unwrap();
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
        let (request, _, _) = sync
            .generate_sync_request()?
            .expect("must have generated a request because there's one list");
        assert!(request.txn_id.is_none());
        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());
        assert!(request.extensions.to_device.since.is_none());

        // If there's a to-device `since` token, we make sure we put the token
        // into the extension config. The rest doesn't need to be re-enabled due to
        // stickyness.
        let since_token = "since";
        sync.update_to_device_since(since_token.to_owned());

        let (request, _, _) = sync
            .generate_sync_request()?
            .expect("must have generated a request because there's one list");

        assert!(request.txn_id.is_none());
        assert!(request.extensions.e2ee.enabled.is_none());
        assert!(request.extensions.to_device.enabled.is_none());
        assert_eq!(request.extensions.to_device.since.as_deref(), Some(since_token));

        Ok(())
    }
}
