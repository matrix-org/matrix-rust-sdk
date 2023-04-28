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
    borrow::BorrowMut,
    collections::BTreeMap,
    fmt::Debug,
    mem,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, RwLock as StdRwLock,
    },
    time::Duration,
};

pub use builder::*;
pub use client::*;
pub use error::*;
use eyeball::unique::Observable;
use futures_core::stream::Stream;
pub use list::*;
use matrix_sdk_base::sync::SyncResponse;
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
use tokio::{spawn, sync::Mutex as AsyncMutex};
use tracing::{debug, error, info_span, instrument, warn, Instrument, Span};
use url::Url;
use uuid::Uuid;

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

    /// The `bump_event_types` field. See
    /// [`SlidingSyncBuilder::bump_event_types`] to learn more.
    bump_event_types: Vec<TimelineEventType>,

    subscriptions: StdRwLock<BTreeMap<OwnedRoomId, v4::RoomSubscription>>,
    unsubscribe: StdRwLock<Vec<OwnedRoomId>>,

    /// Number of times a Sliding Session session has been reset.
    reset_counter: AtomicU8,

    /// The intended state of the extensions being supplied to sliding /sync
    /// calls. May contain the latest next_batch for to_devices, etc.
    extensions: Mutex<Option<ExtensionsConfig>>,
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner), response_handling_lock: Arc::new(AsyncMutex::new(())) }
    }

    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        cache::store_sliding_sync_state(self).await
    }

    /// Create a new [`SlidingSyncBuilder`].
    pub fn builder() -> SlidingSyncBuilder {
        SlidingSyncBuilder::new()
    }

    /// Subscribe to a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn subscribe(&self, room_id: OwnedRoomId, settings: Option<v4::RoomSubscription>) {
        self.inner.subscriptions.write().unwrap().insert(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: OwnedRoomId) {
        if self.inner.subscriptions.write().unwrap().remove(&room_id).is_some() {
            self.inner.unsubscribe.write().unwrap().push(room_id);
        }
    }

    /// Add the common extensions if not already configured.
    pub fn add_common_extensions(&self) {
        let mut lock = self.inner.extensions.lock().unwrap();
        let mut cfg = lock.get_or_insert_with(Default::default);

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

    /// Get access to the SlidingSyncList named `list_name`.
    ///
    /// Note: Remember that this list might have been changed since you started
    /// listening to the stream and is therefor not necessarily up to date
    /// with the lists used for the stream.
    pub fn list(&self, list_name: &str) -> Option<SlidingSyncList> {
        self.inner.lists.read().unwrap().get(list_name).cloned()
    }

    /// Remove the SlidingSyncList named `list_name` from the lists list if
    /// found.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of lists.
    pub fn pop_list(&self, list_name: &String) -> Option<SlidingSyncList> {
        self.inner.lists.write().unwrap().remove(list_name)
    }

    /// Add the list to the list of lists.
    ///
    /// As lists need to have a unique `.name`, if a list with the same name
    /// is found the new list will replace the old one and the return it or
    /// `None`.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of lists.
    pub fn add_list(&self, list: SlidingSyncList) -> Option<SlidingSyncList> {
        self.inner.lists.write().unwrap().insert(list.name().to_owned(), list)
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

    fn prepare_extension_config(&self, pos: Option<&str>) -> ExtensionsConfig {
        if pos.is_none() {
            // The pos is `None`, it's either our initial sync or the proxy forgot about us
            // and sent us an `UnknownPos` error. We need to send out the config for our
            // extensions.
            let mut extensions = self.inner.extensions.lock().unwrap().clone().unwrap_or_default();

            // Always enable to-device events and the e2ee-extension on the initial request,
            // no matter what the caller wants.
            //
            // The to-device `since` parameter is either `None` or guaranteed to be set
            // because the `update_to_device_since()` method updates the
            // self.extensions field and they get persisted to the store using the
            // `cache_to_storage()` method.
            //
            // The token is also loaded from storage in the `SlidingSyncBuilder::build()`
            // method.
            extensions.to_device.enabled = Some(true);
            extensions.e2ee.enabled = Some(true);

            extensions
        } else {
            // We already enabled all the things, just fetch out the to-device since token
            // out of self.extensions and set it in a new, and empty, `ExtensionsConfig`.
            let since = self
                .inner
                .extensions
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|e| e.to_device.since.clone());

            let mut extensions: ExtensionsConfig = Default::default();
            extensions.to_device.since = since;

            extensions
        }
    }

    /// Handle the HTTP response.
    #[instrument(skip_all, fields(lists = lists.len()))]
    fn handle_response(
        &self,
        sliding_sync_response: v4::Response,
        mut sync_response: SyncResponse,
        lists: &mut BTreeMap<String, SlidingSyncList>,
    ) -> Result<UpdateSummary, crate::Error> {
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

        let update_summary = {
            let mut rooms_map = self.inner.rooms.write().unwrap();

            // Update the rooms.
            let mut updated_rooms = Vec::with_capacity(sliding_sync_response.rooms.len());

            for (room_id, mut room_data) in sliding_sync_response.rooms.into_iter() {
                // `sync_response` contains the rooms with decrypted events if any, so look at
                // the timeline events here first if the room exists.
                // Otherwise, let's look at the timeline inside the `sliding_sync_response`.
                let timeline = if let Some(joined_room) = sync_response.rooms.join.remove(&room_id)
                {
                    joined_room.timeline.events
                } else {
                    room_data.timeline.drain(..).map(Into::into).collect()
                };

                if let Some(mut room) = rooms_map.remove(&room_id) {
                    // The room existed before, let's update it.

                    room.update(room_data, timeline);
                    rooms_map.insert(room_id.clone(), room);
                } else {
                    // First time we need this room, let's create it.

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

                updated_rooms.push(room_id);
            }

            // Update the lists.
            let mut updated_lists = Vec::with_capacity(sliding_sync_response.lists.len());

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

            UpdateSummary { lists: updated_lists, rooms: updated_rooms }
        };

        Ok(update_summary)
    }

    async fn sync_once(
        &self,
        stream_id: &str,
        lists: Arc<Mutex<BTreeMap<String, SlidingSyncList>>>,
    ) -> Result<Option<UpdateSummary>> {
        let mut requests_lists = BTreeMap::new();

        {
            let mut lists_lock = lists.lock().unwrap();
            let lists = lists_lock.borrow_mut();

            if lists.is_empty() {
                return Ok(None);
            }

            for (name, list) in lists.iter_mut() {
                requests_lists.insert(name.clone(), list.next_request()?);
            }
        }

        let (pos, delta_token) = {
            let position_lock = self.inner.position.read().unwrap();

            (position_lock.pos.clone(), position_lock.delta_token.clone())
        };

        let room_subscriptions = self.inner.subscriptions.read().unwrap().clone();
        let unsubscribe_rooms = mem::take(&mut *self.inner.unsubscribe.write().unwrap());
        let timeout = Duration::from_secs(30);
        let extensions = self.prepare_extension_config(pos.as_deref());

        debug!("Sending the sliding sync request");

        // Configure long-polling. We need 30 seconds for the long-poll itself, in
        // addition to 30 more extra seconds for the network delays.
        let request_config = RequestConfig::default().timeout(timeout + Duration::from_secs(30));

        // Prepare the request.
        let request = self.inner.client.send_with_homeserver(
            assign!(v4::Request::new(), {
                pos,
                delta_token,
                // We want to track whether the incoming response maps to this
                // request. We use the (optional) `txn_id` field for that.
                txn_id: Some(stream_id.to_owned()),
                timeout: Some(timeout),
                lists: requests_lists,
                bump_event_types: self.inner.bump_event_types.clone(),
                room_subscriptions,
                unsubscribe_rooms,
                extensions,
            }),
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
        let stream_id = stream_id.to_owned();

        // Spawn a new future to ensure that the code inside this future cannot be
        // cancelled if this method is cancelled.
        spawn(async move {
            debug!("Sliding Sync response handling starts");

            // In case the task running this future is detached, we must
            // ensure responses are handled one at a time, hence we lock the
            // `response_handling_lock`.
            let response_handling_lock = this.response_handling_lock.lock().await;

            match &response.txn_id {
                None => {
                    error!(stream_id, "Sliding Sync has received an unexpected response: `txn_id` must match `stream_id`; it's missing");
                }

                Some(txn_id) if txn_id != &stream_id => {
                    error!(
                        stream_id,
                        txn_id,
                        "Sliding Sync has received an unexpected response: `txn_id` must match `stream_id`; they differ"
                    );
                }

                _ => {}
            }

            // Handle and transform a Sliding Sync Response to a `SyncResponse`.
            //
            // We may not need the `sync_response` in the future (once `SyncResponse` will
            // move to Sliding Sync, i.e. to `v4::Response`), but processing the
            // `sliding_sync_response` is vital, so it must be done somewhere; for now it
            // happens here.
            let sync_response = this.inner.client.process_sliding_sync(&response).await?;

            debug!(?sync_response, "Sliding Sync response has been handled by the client");

            let updates = this.handle_response(response, sync_response, lists.lock().unwrap().borrow_mut())?;

            this.cache_to_storage().await?;

            // Release the lock.
            drop(response_handling_lock);

            debug!("Sliding Sync response has been fully handled");

            Ok(Some(updates))
        }).await.unwrap()
    }

    /// Create a _new_ Sliding Sync stream.
    ///
    /// This stream will send requests and will handle responses automatically,
    /// hence updating the lists.
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(name = "sync_stream", skip_all)]
    pub fn stream(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        // Copy all the lists.
        let lists = Arc::new(Mutex::new(self.inner.lists.read().unwrap().clone()));

        // Define a stream ID.
        let stream_id = Uuid::new_v4().to_string();

        debug!(?self.inner.extensions, stream_id, "About to run the sync stream");

        let instrument_span = Span::current();

        async_stream::stream! {
            loop {
                let sync_span = info_span!(parent: &instrument_span, "sync_once");

                sync_span.in_scope(|| {
                    debug!(?self.inner.extensions, "Sync stream loop is running");
                });

                match self.sync_once(&stream_id, lists.clone()).instrument(sync_span.clone()).await {
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
                            if self.inner.reset_counter.fetch_add(1, Ordering::SeqCst) >= MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION {
                                sync_span.in_scope(|| error!("Session expired {MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION} times in a row"));

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

                                    Observable::set(&mut position_lock.pos, None);
                                }

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

    /// Resets the lists.
    pub fn reset_lists(&self) {
        let lists = self.inner.lists.read().unwrap();

        for (_, list) in lists.iter() {
            list.reset();
        }
    }
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
    pos: Observable<Option<String>>,
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
    use assert_matches::assert_matches;
    use ruma::api::client::sync::sync_events::v4::{E2EEConfig, ToDeviceConfig};
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::logged_in_client;

    #[tokio::test]
    async fn to_device_is_enabled_when_pos_is_none() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sync = client.sliding_sync().await.build().await?;
        let extensions = sync.prepare_extension_config(None);

        // If the user doesn't provide any extension config, we enable to-device and
        // e2ee anyways.
        assert_matches!(
            extensions.to_device,
            ToDeviceConfig { enabled: Some(true), since: None, .. }
        );
        assert_matches!(extensions.e2ee, E2EEConfig { enabled: Some(true), .. });

        let some_since = "some_since".to_owned();
        sync.update_to_device_since(some_since.to_owned());
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
}
