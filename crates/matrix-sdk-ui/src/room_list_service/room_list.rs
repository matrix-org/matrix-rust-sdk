// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{future::ready, ops::Deref, sync::Arc};

use async_cell::sync::AsyncCell;
use async_rx::StreamExt as _;
use async_stream::stream;
use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use eyeball_im_util::vector::VectorObserverExt;
use futures_util::{Stream, StreamExt as _, pin_mut, stream};
use matrix_sdk::{
    Client, Room, RoomRecencyStamp, RoomState, SlidingSync, SlidingSyncList,
    executor::{JoinHandle, spawn},
};
use matrix_sdk_base::{RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons};
use ruma::MilliSecondsSinceUnixEpoch;
use tokio::{
    select,
    sync::broadcast::{self, error::RecvError},
};
use tracing::{error, trace};

use super::{
    Error, State,
    filters::BoxedFilterFn,
    sorters::{
        BoxedSorterFn, new_sorter_latest_event, new_sorter_lexicographic, new_sorter_name,
        new_sorter_recency,
    },
};

/// A `RoomList` represents a list of rooms, from a
/// [`RoomListService`](super::RoomListService).
#[derive(Debug)]
pub struct RoomList {
    client: Client,
    sliding_sync_list: SlidingSyncList,
    loading_state: SharedObservable<RoomListLoadingState>,
    loading_state_task: JoinHandle<()>,
}

impl Drop for RoomList {
    fn drop(&mut self) {
        self.loading_state_task.abort();
    }
}

impl RoomList {
    pub(super) async fn new(
        client: &Client,
        sliding_sync: &Arc<SlidingSync>,
        sliding_sync_list_name: &str,
        room_list_service_state: Subscriber<State>,
    ) -> Result<Self, Error> {
        let sliding_sync_list = sliding_sync
            .on_list(sliding_sync_list_name, |list| ready(list.clone()))
            .await
            .ok_or_else(|| Error::UnknownList(sliding_sync_list_name.to_owned()))?;

        let loading_state =
            SharedObservable::new(match sliding_sync_list.maximum_number_of_rooms() {
                Some(maximum_number_of_rooms) => RoomListLoadingState::Loaded {
                    maximum_number_of_rooms: Some(maximum_number_of_rooms),
                },
                None => RoomListLoadingState::NotLoaded,
            });

        Ok(Self {
            client: client.clone(),
            sliding_sync_list: sliding_sync_list.clone(),
            loading_state: loading_state.clone(),
            loading_state_task: spawn(async move {
                pin_mut!(room_list_service_state);

                // As soon as `RoomListService` changes its state, if it isn't
                // `Terminated` nor `Error`, we know we have fetched something,
                // so the room list is loaded.
                while let Some(state) = room_list_service_state.next().await {
                    use State::*;

                    match state {
                        Terminated { .. } | Error { .. } | Init => (),
                        SettingUp | Recovering | Running => break,
                    }
                }

                // Let's jump from `NotLoaded` to `Loaded`.
                let maximum_number_of_rooms = sliding_sync_list.maximum_number_of_rooms();

                loading_state.set(RoomListLoadingState::Loaded { maximum_number_of_rooms });

                // Wait for updates on the maximum number of rooms to update again.
                let mut maximum_number_of_rooms_stream =
                    sliding_sync_list.maximum_number_of_rooms_stream();

                while let Some(maximum_number_of_rooms) =
                    maximum_number_of_rooms_stream.next().await
                {
                    loading_state.set(RoomListLoadingState::Loaded { maximum_number_of_rooms });
                }
            }),
        })
    }

    /// Get a subscriber to the room list loading state.
    ///
    /// This method will send out the current loading state as the first update.
    ///
    /// See [`RoomListLoadingState`].
    pub fn loading_state(&self) -> Subscriber<RoomListLoadingState> {
        self.loading_state.subscribe_reset()
    }

    /// Get a stream of rooms.
    fn entries(&self) -> (Vector<Room>, impl Stream<Item = Vec<VectorDiff<Room>>> + '_) {
        self.client.rooms_stream()
    }

    /// Get a configurable stream of rooms.
    ///
    /// It's possible to provide a filter that will filter out room list
    /// entries, and that it's also possible to “paginate” over the entries by
    /// `page_size`. The rooms are also sorted.
    ///
    /// The returned stream will only start yielding diffs once a filter is set
    /// through the returned [`RoomListDynamicEntriesController`]. For every
    /// call to [`RoomListDynamicEntriesController::set_filter`], the stream
    /// will yield a [`VectorDiff::Reset`] followed by any updates of the
    /// room list under that filter (until the next reset).
    pub fn entries_with_dynamic_adapters(
        &self,
        page_size: usize,
    ) -> (impl Stream<Item = Vec<VectorDiff<RoomListItem>>> + '_, RoomListDynamicEntriesController)
    {
        self.entries_with_dynamic_adapters_impl(page_size, false)
    }

    #[doc(hidden)]
    pub fn entries_with_dynamic_adapters_with(
        &self,
        page_size: usize,
        enable_latest_event_sorter: bool,
    ) -> (impl Stream<Item = Vec<VectorDiff<RoomListItem>>> + '_, RoomListDynamicEntriesController)
    {
        self.entries_with_dynamic_adapters_impl(page_size, enable_latest_event_sorter)
    }

    fn entries_with_dynamic_adapters_impl(
        &self,
        page_size: usize,
        enable_latest_event_sorter: bool,
    ) -> (impl Stream<Item = Vec<VectorDiff<RoomListItem>>> + '_, RoomListDynamicEntriesController)
    {
        let room_info_notable_update_receiver = self.client.room_info_notable_update_receiver();
        let list = self.sliding_sync_list.clone();

        let filter_fn_cell = AsyncCell::shared();

        let limit = SharedObservable::<usize>::new(page_size);
        let limit_stream = limit.subscribe();

        let dynamic_entries_controller = RoomListDynamicEntriesController::new(
            filter_fn_cell.clone(),
            page_size,
            limit,
            list.maximum_number_of_rooms_stream(),
        );

        let stream = stream! {
            loop {
                let filter_fn = filter_fn_cell.take().await;

                let (raw_values, raw_stream) = self.entries();
                let values = raw_values.into_iter().map(Into::into).collect::<Vector<RoomListItem>>();

                // Combine normal stream events with other updates from rooms
                let stream = merge_stream_and_receiver(values.clone(), raw_stream, room_info_notable_update_receiver.resubscribe());

                let mut sorters: Vec<BoxedSorterFn> = Vec::with_capacity(3);

                if enable_latest_event_sorter {
                    // Sort by latest event's kind, i.e. put the rooms with a
                    // **local** latest event first.
                    sorters.push(Box::new(new_sorter_latest_event()));
                }

                // Sort rooms by their recency (either by looking
                // at their latest event's timestamp, or their
                // `recency_stamp`).
                sorters.push(Box::new(new_sorter_recency()));
                // Finally, sort by name.
                sorters.push(Box::new(new_sorter_name()));

                let (values, stream) = (values, stream)
                    .filter(filter_fn)
                    .sort_by(new_sorter_lexicographic(sorters))
                    .dynamic_head_with_initial_value(page_size, limit_stream.clone());

                // Clearing the stream before chaining with the real stream.
                yield stream::once(ready(vec![VectorDiff::Reset { values }]))
                    .chain(stream);
            }
        }
        .switch();

        (stream, dynamic_entries_controller)
    }
}

/// This function remembers the current state of the unfiltered room list, so it
/// knows where all rooms are. When the receiver is triggered, a Set operation
/// for the room position is inserted to the stream.
fn merge_stream_and_receiver(
    mut current_values: Vector<RoomListItem>,
    raw_stream: impl Stream<Item = Vec<VectorDiff<Room>>>,
    mut room_info_notable_update_receiver: broadcast::Receiver<RoomInfoNotableUpdate>,
) -> impl Stream<Item = Vec<VectorDiff<RoomListItem>>> {
    stream! {
        pin_mut!(raw_stream);

        loop {
            select! {
                // We want to give priority on updates from `raw_stream` as it will necessarily trigger a “refresh” of the rooms.
                biased;

                diffs = raw_stream.next() => {
                    if let Some(diffs) = diffs {
                        let diffs = diffs.into_iter().map(|diff| diff.map(RoomListItem::from)).collect::<Vec<_>>();

                        for diff in &diffs {
                            diff.clone().map(|room| {
                                trace!(room = %room.room_id(), "updated in response");
                                room
                            }).apply(&mut current_values);
                        }

                        yield diffs;
                    } else {
                        // Restart immediately, don't keep on waiting for the receiver
                        break;
                    }
                }

                update = room_info_notable_update_receiver.recv() => {
                    match update {
                        Ok(update) => {
                            // Filter which _reason_ can trigger an update of
                            // the room list.
                            //
                            // If the update is strictly about the
                            // `RECENCY_STAMP`, let's ignore it, because the
                            // Latest Event type is used to sort the room list
                            // by recency already. We don't want to trigger an
                            // update because of `RECENCY_STAMP`.
                            //
                            // If the update contains more reasons than
                            // `RECENCY_STAMP`, then it's fine. That's why we
                            // are using `==` instead of `contains`.
                            if update.reasons == RoomInfoNotableUpdateReasons::RECENCY_STAMP {
                                continue;
                            }

                            // Emit a `VectorDiff::Set` for the specific rooms.
                            if let Some(index) = current_values.iter().position(|room| room.room_id() == update.room_id) {
                                let mut room = current_values[index].clone();
                                room.refresh_cached_data();

                                yield vec![VectorDiff::Set { index, value: room }];
                            }
                        }

                        Err(RecvError::Closed) => {
                            error!("Cannot receive room info notable updates because the sender has been closed");

                            break;
                        }

                        Err(RecvError::Lagged(n)) => {
                            error!(number_of_missed_updates = n, "Lag when receiving room info notable update");
                        }
                    }
                }
            }
        }
    }
}

/// The loading state of a [`RoomList`].
///
/// When a [`RoomList`] is displayed to the user, it can be in various states.
/// This enum tries to represent those states with a correct level of
/// abstraction.
///
/// See [`RoomList::loading_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomListLoadingState {
    /// The [`RoomList`] has not been loaded yet, i.e. a sync might run
    /// or not run at all, there is nothing to show in this `RoomList` yet.
    /// It's a good opportunity to show a placeholder to the user.
    ///
    /// From [`Self::NotLoaded`], it's only possible to move to
    /// [`Self::Loaded`].
    NotLoaded,

    /// The [`RoomList`] has been loaded, i.e. a sync has been run, or more
    /// syncs are running, there is probably something to show to the user.
    /// Either the user has 0 room, in this case, it's a good opportunity to
    /// show a special screen for that, or the user has multiple rooms, and it's
    /// the classical room list.
    ///
    /// The number of rooms is represented by `maximum_number_of_rooms`.
    ///
    /// From [`Self::Loaded`], it's not possible to move back to
    /// [`Self::NotLoaded`].
    Loaded {
        /// The maximum number of rooms a [`RoomList`] contains.
        ///
        /// It does not mean that there are exactly this many rooms to display.
        /// The room entries are represented by [`RoomListItem`]. The room entry
        /// might have been synced or not synced yet, but we know for sure
        /// (from the server), that there will be this amount of rooms in the
        /// list at the end.
        ///
        /// Note that it's an `Option`, because it may be possible that the
        /// server did miss to send us this value. It's up to you, dear reader,
        /// to know which default to adopt in case of `None`.
        maximum_number_of_rooms: Option<u32>,
    },
}

/// Controller for the [`RoomList`] dynamic entries.
///
/// To get one value of this type, use
/// [`RoomList::entries_with_dynamic_adapters`]
pub struct RoomListDynamicEntriesController {
    filter: Arc<AsyncCell<BoxedFilterFn>>,
    page_size: usize,
    limit: SharedObservable<usize>,
    maximum_number_of_rooms: Subscriber<Option<u32>>,
}

impl RoomListDynamicEntriesController {
    fn new(
        filter: Arc<AsyncCell<BoxedFilterFn>>,
        page_size: usize,
        limit_stream: SharedObservable<usize>,
        maximum_number_of_rooms: Subscriber<Option<u32>>,
    ) -> Self {
        Self { filter, page_size, limit: limit_stream, maximum_number_of_rooms }
    }

    /// Set the filter.
    ///
    /// If the associated stream has been dropped, returns `false` to indicate
    /// the operation didn't have an effect.
    pub fn set_filter(&self, filter: BoxedFilterFn) -> bool {
        if Arc::strong_count(&self.filter) == 1 {
            // there is no other reference to the boxed filter fn, setting it
            // would be pointless (no new references can be created from self,
            // either)
            false
        } else {
            self.filter.set(filter);
            true
        }
    }

    /// Add one page, i.e. view `page_size` more entries in the room list if
    /// any.
    pub fn add_one_page(&self) {
        let Some(max) = self.maximum_number_of_rooms.get() else {
            return;
        };

        let max: usize = max.try_into().unwrap();
        let limit = self.limit.get();

        if limit < max {
            // With this logic, it is possible that `limit` becomes greater than `max` if
            // `max - limit < page_size`, and that's perfectly fine. It's OK to have a
            // `limit` greater than `max`, but it's not OK to increase the limit
            // indefinitely.
            self.limit.set_if_not_eq(limit + self.page_size);
        }
    }

    /// Reset the one page, i.e. forget all pages and move back to the first
    /// page.
    pub fn reset_to_one_page(&self) {
        self.limit.set_if_not_eq(self.page_size);
    }
}

/// A facade type that derefs to [`Room`] and that caches data from
/// [`RoomInfo`].
///
/// Why caching data? [`RoomInfo`] is behind a lock. Every time a filter or a
/// sorter calls a method on [`Room`], it's likely to hit the lock in front of
/// [`RoomInfo`]. It creates a big contention. By caching the data, it avoids
/// hitting the lock, improving the performance greatly.
///
/// Data are refreshed in `merge_stream_and_receiver` (private function).
///
/// [`RoomInfo`]: matrix_sdk::RoomInfo
#[derive(Clone, Debug)]
pub struct RoomListItem {
    /// The inner room.
    inner: Room,

    /// Cache of `Room::latest_event_timestamp`.
    pub(super) cached_latest_event_timestamp: Option<MilliSecondsSinceUnixEpoch>,

    /// Cache of `Room::latest_event_is_unsent`.
    pub(super) cached_latest_event_is_unsent: bool,

    /// Cache of `Room::recency_stamp`.
    pub(super) cached_recency_stamp: Option<RoomRecencyStamp>,

    /// Cache of `Room::cached_display_name`, already as a string.
    pub(super) cached_display_name: Option<String>,

    /// Cache of `Room::is_space`.
    pub(super) cached_is_space: bool,

    // Cache of `Room::state`.
    pub(super) cached_state: RoomState,
}

impl RoomListItem {
    /// Deconstruct to the inner room value.
    pub fn into_inner(self) -> Room {
        self.inner
    }

    /// Refresh the cached data.
    pub(super) fn refresh_cached_data(&mut self) {
        self.cached_latest_event_timestamp = self.inner.latest_event_timestamp();
        self.cached_latest_event_is_unsent = self.inner.latest_event_is_unsent();
        self.cached_recency_stamp = self.inner.recency_stamp();
        self.cached_display_name = self.inner.cached_display_name().map(|name| name.to_string());
        self.cached_is_space = self.inner.is_space();
        self.cached_state = self.inner.state();
    }
}

impl From<Room> for RoomListItem {
    fn from(inner: Room) -> Self {
        let cached_latest_event_timestamp = inner.latest_event_timestamp();
        let cached_latest_event_is_unsent = inner.latest_event_is_unsent();
        let cached_recency_stamp = inner.recency_stamp();
        let cached_display_name = inner.cached_display_name().map(|name| name.to_string());
        let cached_is_space = inner.is_space();
        let cached_state = inner.state();

        Self {
            inner,
            cached_latest_event_timestamp,
            cached_latest_event_is_unsent,
            cached_recency_stamp,
            cached_display_name,
            cached_is_space,
            cached_state,
        }
    }
}

impl Deref for RoomListItem {
    type Target = Room;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
