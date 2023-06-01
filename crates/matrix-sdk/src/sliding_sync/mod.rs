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
        Arc, RwLock as StdRwLock,
    },
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
    assign,
    events::TimelineEventType,
    OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select, spawn,
    sync::{broadcast::Sender, Mutex as AsyncMutex},
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

/// TODO common parts of the two loops
#[derive(Clone, Debug)]
struct CommonLoopComponents {
    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    /// The HTTP Matrix client.
    client: Client,

    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    /// Internal channel used to pass messages between Sliding Sync and other
    /// types.
    internal_channel: Sender<SlidingSyncInternalMessage>,

    /// Static configuration for extensions, passed in the slidinc sync
    /// requests.
    extensions: ExtensionsConfig,

    /// Room subscriptions, i.e. rooms that may be out-of-scope of all lists but
    /// one wants to receive updates.
    room_subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,

    /// Rooms to unsubscribe, see [`Self::room_subscriptions`].
    room_unsubscriptions: BTreeSet<OwnedRoomId>,
}

impl CommonLoopComponents {
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

#[derive(Clone, Debug)]
pub struct SlidingSyncListLoop {
    /// The lists of this Sliding Sync instance.
    lists: BTreeMap<String, SlidingSyncList>,

    /// The rooms details
    rooms: BTreeMap<OwnedRoomId, SlidingSyncRoom>,

    common: CommonLoopComponents,
}

impl SlidingSyncListLoop {
    /// Lookup a specific room
    pub fn get_room(&self, room_id: &RoomId) -> Option<SlidingSyncRoom> {
        self.rooms.get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.rooms.len()
    }

    /// Find a list by its name, and do something on it if it exists.
    pub fn on_list<F, R>(&self, list_name: &str, f: F) -> Option<R>
    where
        F: FnOnce(&SlidingSyncList) -> R,
    {
        self.lists.get(list_name).map(f)
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        room_ids.map(|room_id| self.rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.rooms.values().cloned().collect()
    }

    /// Add the list to the list of lists.
    ///
    /// As lists need to have a unique `.name`, if a list with the same name
    /// is found the new list will replace the old one and the return it or
    /// `None`.
    pub async fn add_list(
        &mut self,
        list_builder: SlidingSyncListBuilder,
    ) -> Result<Option<SlidingSyncList>> {
        let list = list_builder.build(self.common.internal_channel.clone());

        let old_list = self.lists.insert(list.name().to_owned(), list);

        self.common.internal_channel_send_if_possible(
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
        &mut self,
        mut list_builder: SlidingSyncListBuilder,
    ) -> Result<Option<SlidingSyncList>> {
        let Some(ref storage_key) = self.common.storage_key else {
            return Err(error::Error::MissingStorageKeyForCaching.into());
        };

        let reloaded_rooms =
            list_builder.set_cached_and_reload(&self.common.client, storage_key).await?;

        if !reloaded_rooms.is_empty() {
            for (key, frozen) in reloaded_rooms {
                self.rooms.entry(key).or_insert_with(|| {
                    SlidingSyncRoom::from_frozen(frozen, self.common.client.clone())
                });
            }
        }

        self.add_list(list_builder).await
    }
}

#[derive(Clone, Debug)]
pub struct SlidingSyncNotificationLoop {
    to_device_token: Option<String>,

    common: CommonLoopComponents,
}

impl SlidingSyncNotificationLoop {
    fn update_to_device_since(&mut self, since: String) {
        self.to_device_token = Some(since);
    }
}

/// What's the role of this sliding sync loop?
#[derive(Clone, Debug)]
pub enum SlidingSyncRole {
    Lists(SlidingSyncListLoop),

    Notifications(SlidingSyncNotificationLoop),
}

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
    /// Position markers
    position: StdRwLock<SlidingSyncPositionMarkers>,

    /// The `bump_event_types` field. See
    /// [`SlidingSyncBuilder::bump_event_types`] to learn more.
    bump_event_types: Vec<TimelineEventType>,

    /// Number of times a Sliding Sync session has been reset.
    reset_counter: AtomicU8,

    /// TODO
    role: StdRwLock<SlidingSyncRole>,
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner), response_handling_lock: Arc::new(AsyncMutex::new(())) }
    }

    async fn cache_to_storage(&self) -> Result<()> {
        cache::store_sliding_sync_state(self).await
    }

    pub fn new_list_loop(client: Client) -> SlidingSyncListLoopBuilder {
        SlidingSyncListLoopBuilder::new(client)
    }

    pub fn new_notification_loop(client: Client) -> SlidingSyncNotificationLoopBuilder {
        SlidingSyncNotificationLoopBuilder::new(client)
    }

    /// Subscribe to a given room.
    pub fn subscribe_to_room(&self, room_id: OwnedRoomId, settings: Option<v4::RoomSubscription>) {
        let common = self.common_mut();

        common.room_subscriptions.insert(room_id, settings.unwrap_or_default());

        common.internal_channel_send_if_possible(
            SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
        );
    }

    /// Unsubscribe from a given room.
    pub fn unsubscribe_from_room(&self, room_id: OwnedRoomId) {
        let common = self.common_mut();

        // If removing the subscription was successful…
        if common.room_subscriptions.remove(&room_id).is_some() {
            // … then keep the unsubscription for the next request.
            common.room_unsubscriptions.insert(room_id);

            common.internal_channel_send_if_possible(
                SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
            );
        }
    }

    /// TODO
    pub fn as_list_loop(&self) -> Option<&SlidingSyncListLoop> {
        let role = self.inner.role.read().unwrap();
        // TODO gnurf borrow checker
        match &*role {
            SlidingSyncRole::Lists(list) => Some(list),
            SlidingSyncRole::Notifications(..) => None,
        }
    }

    /// TODO
    pub fn as_list_loop_mut(&self) -> Option<&mut SlidingSyncListLoop> {
        let mut role = self.inner.role.write().unwrap();
        match &mut *role {
            SlidingSyncRole::Lists(list) => Some(list),
            SlidingSyncRole::Notifications(..) => None,
        }
    }

    /// TODO
    pub fn as_notification_loop(&self) -> Option<&SlidingSyncNotificationLoop> {
        let role = self.inner.role.read().unwrap();
        match &*role {
            SlidingSyncRole::Lists(..) => None,
            SlidingSyncRole::Notifications(notifications) => Some(notifications),
        }
    }

    /// TODO
    fn common(&self) -> &CommonLoopComponents {
        let role = self.inner.role.read().unwrap();
        match &*role {
            SlidingSyncRole::Lists(l) => &l.common,
            SlidingSyncRole::Notifications(n) => &n.common,
        }
    }

    /// TODO
    fn common_mut(&self) -> &mut CommonLoopComponents {
        let mut role = self.inner.role.write().unwrap();
        match &mut *role {
            SlidingSyncRole::Lists(l) => &mut l.common,
            SlidingSyncRole::Notifications(n) => &mut n.common,
        }
    }

    fn prepare_extension_config(&self, pos: Option<&str>) -> ExtensionsConfig {
        let mut extensions = self.common().extensions.clone();

        // Note that applies to both arms: when the pos is `None`, it's either our initial sync or the proxy forgot about us
        // and sent us an `UnknownPos` error. We need to send out the config for our
        // extensions.
        // TODO to be invalidated by the sticky parameters PR 🙃

        let role = self.inner.role.read().unwrap();
        match &*role {
            SlidingSyncRole::Lists(_) => {
                // We make sure to not enable the e2ee and to-device extensions for the list loop, so it doesn't conflict with the notification loop.
                if pos.is_none() {
                    extensions.e2ee.enabled = Some(false);
                    extensions.to_device.enabled = Some(false);
                }
            }

            SlidingSyncRole::Notifications(notification_loop) => {
                // For the notification loop, force-enabling the e2ee extensions and to-device extension.
                if pos.is_none() {
                    extensions.e2ee.enabled = Some(true);
                    extensions.to_device.enabled = Some(true);
                }

                // Try to chime in a to-device token that may be unset or restored from the
                // cache.
                let to_device_since = notification_loop.to_device_token.clone();
                extensions.to_device.since = to_device_since;
            }
        }

        extensions
    }

    /// Handle the HTTP response.
    #[instrument(skip_all, fields(lists))]
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
            self.common().client.process_sliding_sync(&sliding_sync_response).await?;

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

        let update_summary = {
            let mut role = self.inner.role.write().unwrap();

            match &mut *role {
                SlidingSyncRole::Lists(list_loop) => {
                    // Update the rooms.
                    let updated_rooms = {
                        let rooms_map = &mut list_loop.rooms;

                        let mut updated_rooms =
                            Vec::with_capacity(sliding_sync_response.rooms.len());

                        for (room_id, mut room_data) in sliding_sync_response.rooms.into_iter() {
                            // `sync_response` contains the rooms with decrypted events if any, so look at
                            // the timeline events here first if the room exists.
                            // Otherwise, let's look at the timeline inside the `sliding_sync_response`.
                            let timeline = if let Some(joined_room) =
                                sync_response.rooms.join.remove(&room_id)
                            {
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
                                            list_loop.common.client.clone(),
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
                        let mut updated_lists =
                            Vec::with_capacity(sliding_sync_response.lists.len());
                        let lists = &mut list_loop.lists;

                        for (name, updates) in sliding_sync_response.lists {
                            let Some(list) = lists.get_mut(&name) else {
                        error!("Response for list `{name}` - unknown to us; skipping");

                        continue;
                    };

                            let maximum_number_of_rooms: u32 = updates
                                .count
                                .try_into()
                                .expect("failed to convert `count` to `u32`");

                            if list.update(maximum_number_of_rooms, &updates.ops, &updated_rooms)? {
                                updated_lists.push(name.clone());
                            }
                        }

                        updated_lists
                    };

                    UpdateSummary { lists: updated_lists, rooms: updated_rooms }
                }

                SlidingSyncRole::Notifications(notification_loop) => {
                    // Update the `to-device` next-batch if any.
                    if let Some(to_device) = sliding_sync_response.extensions.to_device {
                        notification_loop.to_device_token = Some(to_device.next_batch);
                    }

                    UpdateSummary::default()
                }
            }
        };

        Ok(update_summary)
    }

    async fn sync_once(&self) -> Result<Option<UpdateSummary>> {
        let (request, request_config, requested_room_unsubscriptions) = {
            // Collect requests for lists.
            let mut requests_lists = BTreeMap::new();

            {
                let mut role = self.inner.role.write().unwrap();

                match &mut *role {
                    SlidingSyncRole::Lists(list_loop) => {
                        if list_loop.lists.is_empty() {
                            return Ok(None);
                        }

                        for (name, list) in list_loop.lists.iter_mut() {
                            requests_lists.insert(name.clone(), list.next_request()?);
                        }
                    }

                    SlidingSyncRole::Notifications(notification_loop) => {
                        // TODO ?
                    }
                }
            }

            // Collect the `pos` and `delta_token`.
            let (pos, delta_token) = {
                let position_lock = self.inner.position.read().unwrap();

                (position_lock.pos.clone(), position_lock.delta_token.clone())
            };

            Span::current().record("pos", &pos);

            // Collect other data.
            let room_subscriptions = self.common().room_subscriptions.clone();
            let room_unsubscriptions = self.common().room_unsubscriptions.clone();
            let timeout = Duration::from_secs(30);

            let extensions = self.prepare_extension_config(pos.as_deref());

            (
                // Build the request itself.
                assign!(v4::Request::new(), {
                    pos,
                    delta_token,
                    timeout: Some(timeout),
                    lists: requests_lists,
                    bump_event_types: self.inner.bump_event_types.clone(),
                    room_subscriptions,
                    unsubscribe_rooms: room_unsubscriptions.iter().cloned().collect(),
                    extensions,
                }),
                // Configure long-polling. We need 30 seconds for the long-poll itself, in
                // addition to 30 more extra seconds for the network delays.
                RequestConfig::default().timeout(timeout + Duration::from_secs(30)),
                room_unsubscriptions,
            )
        };

        debug!("Sending the sliding sync request");

        // Prepare the request.
        let common = self.common();
        let request = common.client.send_with_homeserver(
            request,
            Some(request_config),
            common.homeserver.as_ref().map(ToString::to_string),
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
                futures_util::future::join(common.client.send_outgoing_requests(), request).await;

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
                let room_unsubscriptions = &mut this.common_mut().room_unsubscriptions;

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

    /// Create a _new_ Sliding Sync sync-loop.
    ///
    /// This method returns a `Stream`, which will send requests and will handle
    /// responses automatically. Lists and rooms are updated automatically.
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(name = "sync_stream", skip_all)]
    pub fn sync(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        // debug!(?self.common().extensions, ?self.inner.position, "About to run the sync-loop");

        let sync_span = Span::current();
        let mut internal_channel_receiver = self.common().internal_channel.subscribe();

        stream! {
            loop {
                // sync_span.in_scope(|| {
                    // debug!(?self.inner.extensions, ?self.inner.position,"Sync-loop is running");
                // });

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
                                        sync_span.in_scope(|| {
                                            error!("Session expired {MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION} times in a row");
                                        });

                                        // The session has expired too many times, let's raise an error!
                                        yield Err(error);

                                        break;
                                    }

                                    // Let's reset the Sliding Sync session.
                                    sync_span.in_scope(|| {
                                        warn!("Session expired. Restarting Sliding Sync.");

                                        // To “restart” a Sliding Sync session, we set `pos` to its initial value.
                                        {
                                            let mut position_lock = self.inner.position.write().unwrap();
                                            position_lock.pos = None;
                                        }

                                        // debug!(?self.inner.extensions, ?self.inner.position, "Sliding Sync has been reset");
                                    });
                                }

                                yield Err(error);

                                continue;
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
        Ok(self.common().internal_channel_send(SlidingSyncInternalMessage::SyncLoopStop)?)
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
}

#[derive(Debug)]
pub(super) struct SlidingSyncPositionMarkers {
    pos: Option<String>,
    delta_token: Option<String>,
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
        let position = sliding_sync.inner.position.read().unwrap();

        let to_device_since = sliding_sync
            .as_notification_loop()
            .and_then(|notification_loop| notification_loop.to_device_token.clone());

        FrozenSlidingSync { delta_token: position.delta_token.clone(), to_device_since }
    }
}

/// A summary of the updates received after a sync (like in
/// [`SlidingSync::sync`]).
#[derive(Debug, Default, Clone)]
pub struct UpdateSummary {
    /// The names of the lists that have seen an update.
    pub lists: Vec<String>,
    /// The rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use futures_util::{pin_mut, StreamExt};
    use ruma::{
        api::client::sync::sync_events::v4::{E2EEConfig, ToDeviceConfig},
        room_id,
    };
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::logged_in_client;

    #[tokio::test]
    async fn to_device_is_enabled_when_pos_is_none() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sync = client.sliding_sync_list_loop().build().await?;
        let extensions = sync.prepare_extension_config(None);

        // If the user doesn't provide any extension config, we enable to-device and
        // e2ee anyways.
        assert_matches!(
            extensions.to_device,
            ToDeviceConfig { enabled: Some(true), since: None, .. }
        );
        assert_matches!(extensions.e2ee, E2EEConfig { enabled: Some(true), .. });

        let some_since = "some_since".to_owned();
        todo!();
        // sync.update_to_device_since(some_since.to_owned());
        let extensions = sync.prepare_extension_config(Some("foo"));

        // If there's a `pos` and to-device `since` token, we make sure we put the token
        // into the extension config. The rest doesn't need to be re-enabled due to
        // stickyness.
        assert_matches!(
            extensions.to_device,
            ToDeviceConfig { enabled: None, since: Some(since), .. } if since == some_since
        );
        assert_matches!(extensions.e2ee, E2EEConfig { enabled: None, .. });

        let extensions = sync.prepare_extension_config(None);
        // Even if there isn't a `pos`, if we have a to-device `since` token, we put it
        // into the request.
        assert_matches!(
            extensions.to_device,
            ToDeviceConfig { enabled: Some(true), since: Some(since), .. } if since == some_since
        );

        Ok(())
    }

    async fn new_sliding_sync(
        lists: Vec<SlidingSyncListBuilder>,
    ) -> Result<(MockServer, SlidingSync)> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut sliding_sync_builder = client.sliding_sync_list_loop();

        for list in lists {
            sliding_sync_builder = sliding_sync_builder.add_list(list);
        }

        let sliding_sync = sliding_sync_builder.build().await?;

        Ok((server, sliding_sync))
    }

    #[tokio::test]
    async fn test_subscribe_to_room() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        let _stream = sliding_sync.sync();
        pin_mut!(_stream);

        let room0 = room_id!("!r0:bar.org").to_owned();
        let room1 = room_id!("!r1:bar.org").to_owned();
        let room2 = room_id!("!r2:bar.org").to_owned();

        sliding_sync.subscribe_to_room(room0.clone(), None);
        sliding_sync.subscribe_to_room(room1.clone(), None);

        {
            let room_subscriptions = &sliding_sync.common().room_subscriptions;

            assert!(room_subscriptions.contains_key(&room0));
            assert!(room_subscriptions.contains_key(&room1));
            assert!(!room_subscriptions.contains_key(&room2));
        }

        sliding_sync.unsubscribe_from_room(room0.clone());
        sliding_sync.unsubscribe_from_room(room2.clone());

        {
            let room_subscriptions = &sliding_sync.common().room_subscriptions;

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
    async fn test_to_device_token_properly_cached() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        // When no to-device token is present, `prepare_extensions_config` doesn't fill
        // the request with it.
        let config = sliding_sync.prepare_extension_config(Some("pos"));
        assert!(config.to_device.since.is_none());

        let config = sliding_sync.prepare_extension_config(None);
        assert!(config.to_device.since.is_none());

        // When no to-device token is present, it's still not there after caching
        // either.
        let frozen = FrozenSlidingSync::from(&sliding_sync);
        assert!(frozen.to_device_since.is_none());

        // When a to-device token is present, `prepare_extensions_config` fills the
        // request with it.
        let since = String::from("my-to-device-since-token");
        // TODO
        // sliding_sync.update_to_device_since(since.clone());

        let config = sliding_sync.prepare_extension_config(Some("pos"));
        assert_eq!(config.to_device.since.as_ref(), Some(&since));

        let config = sliding_sync.prepare_extension_config(None);
        assert_eq!(config.to_device.since.as_ref(), Some(&since));

        let frozen = FrozenSlidingSync::from(&sliding_sync);
        assert_eq!(frozen.to_device_since, Some(since));

        Ok(())
    }

    #[tokio::test]
    async fn test_add_list() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        let _stream = sliding_sync.sync();
        pin_mut!(_stream);

        sliding_sync
            .as_list_loop_mut()
            .unwrap()
            .add_list(
                SlidingSyncList::builder("bar")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(50..=60)),
            )
            .await?;

        let lists = &sliding_sync.as_list_loop().unwrap().lists;

        assert!(lists.contains_key("foo"));
        assert!(lists.contains_key("bar"));

        // this test also ensures that Tokio is not panicking when calling `add_list`.

        Ok(())
    }

    #[tokio::test]
    async fn test_stop_sync_loop() -> Result<()> {
        let (_server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
        .await?;

        // Start the sync-loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync-loop is actually running.
        for _ in 0..3 {
            assert!(stream.next().await.is_some());
        }

        // Stop the sync-loop.
        sliding_sync.stop_sync()?;

        // The sync-loop is actually stopped.
        for _ in 0..3 {
            assert!(stream.next().await.is_none());
        }

        // Start a new sync-loop.
        let stream = sliding_sync.sync();
        pin_mut!(stream);

        // The sync-loop is actually running.
        for _ in 0..3 {
            assert!(stream.next().await.is_some());
        }

        Ok(())
    }
}
