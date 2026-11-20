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

use std::{cmp::Reverse, collections::BTreeSet, future::ready, ops::Deref, pin::pin, sync::Arc};

use async_cell::sync::AsyncCell;
use async_rx::StreamExt as _;
use async_stream::stream;
use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use eyeball_im_util::vector::VectorObserverExt;
use futures_util::{Stream, StreamExt as _, stream};
use matrix_sdk::{
    Client, Room, RoomRecencyStamp, RoomState, SlidingSync, SlidingSyncList,
    task_monitor::BackgroundTaskHandle,
};
use matrix_sdk_base::RoomInfoNotableUpdate;
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
        new_sorter_latest_event, new_sorter_lexicographic, new_sorter_name, new_sorter_recency,
    },
};

/// A `RoomList` represents a list of rooms, from a
/// [`RoomListService`](super::RoomListService).
#[derive(Debug)]
pub struct RoomList {
    client: Client,
    sliding_sync_list: SlidingSyncList,
    loading_state: SharedObservable<RoomListLoadingState>,
    _loading_state_task: BackgroundTaskHandle,
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
            _loading_state_task: client
                .task_monitor()
                .spawn_infinite_task("room_list::loading_state_task", async move {
                    let mut room_list_service_state = pin!(room_list_service_state);

                    // As soon as `RoomListService` changes its state, if it
                    // isn't `Terminated` nor `Error`, we know we have fetched
                    // something, so the room list is loaded.
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

                    // Wait for updates on the maximum number of rooms to update
                    // again.
                    let mut maximum_number_of_rooms_stream =
                        sliding_sync_list.maximum_number_of_rooms_stream();

                    while let Some(maximum_number_of_rooms) =
                        maximum_number_of_rooms_stream.next().await
                    {
                        loading_state.set(RoomListLoadingState::Loaded { maximum_number_of_rooms });
                    }
                })
                .abort_on_drop(),
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
    /// will yield a [`VectorDiff::Reset`] followed by any updates of the room
    /// list under that filter (until the next reset).
    pub fn entries_with_dynamic_adapters(
        &self,
        page_size: usize,
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
                let mut values = raw_values.into_iter().map(Into::into).collect::<Vector<RoomListItem>>();
                backfill_timestamps(&mut values);

                // Combine normal stream events with other updates from rooms
                let stream = merge_stream_and_receiver(values.clone(), raw_stream, room_info_notable_update_receiver.resubscribe());

                let (values, stream) = (values, stream)
                    .filter(filter_fn)
                    .sort_by(new_sorter_lexicographic(vec![
                        // Sort by latest event's kind, i.e. put the rooms with
                        // a **local** latest event first.
                        Box::new(new_sorter_latest_event()),
                        // Sort rooms by their recency (either by looking at
                        // their latest event's timestamp, or their
                        // `recency_stamp`).
                        Box::new(new_sorter_recency()),
                        // Finally, sort by name.
                        Box::new(new_sorter_name()),
                    ]))
                    .dynamic_head_with_initial_value(page_size, limit_stream.clone());

                // Clearing the stream before chaining with the real stream.
                yield stream::once(ready(vec![VectorDiff::Reset { values }]))
                    .chain(stream);
            }
        }
        .fuse()
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
        let mut raw_stream = pin!(raw_stream);

        loop {
            select! {
                // We want to give priority on updates from `raw_stream` as it will necessarily trigger a “refresh” of the rooms.
                biased;

                diffs = raw_stream.next() => {
                    if let Some(diffs) = diffs {
                        let mut diffs = diffs.into_iter().map(|diff| diff.map(RoomListItem::from)).collect::<Vec<_>>();

                        for diff in &diffs {
                            diff.clone().map(|room| {
                                trace!(room = %room.room_id(), "updated in response");
                                room
                            }).apply(&mut current_values);
                        }

                        for index in backfill_timestamps(&mut current_values) {
                            diffs.push(VectorDiff::Set { index, value: current_values[index].clone() });
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
                            // Drain every update already sitting in the
                            // channel, so that a burst of updates (e.g. a
                            // batch of freshly computed latest events, or the
                            // per-room updates of one sync response) is
                            // processed as a single batch of `VectorDiff`s.
                            // Each room is kept once, in arrival order.
                            let mut room_ids = Vec::new();
                            let mut seen = std::collections::HashSet::new();
                            let mut add = |update: RoomInfoNotableUpdate| {
                                if seen.insert(update.room_id.clone()) {
                                    room_ids.push(update.room_id);
                                }
                            };

                            add(update);

                            loop {
                                use broadcast::error::TryRecvError;

                                match room_info_notable_update_receiver.try_recv() {
                                    Ok(update) => add(update),
                                    Err(TryRecvError::Lagged(n)) => {
                                        error!(number_of_missed_updates = n, "Lag when receiving room info notable update");
                                    }
                                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                                }
                            }

                            // Emit a `VectorDiff::Set` for each updated room,
                            // and for each room whose back-filled timestamp
                            // changed as a consequence.
                            let mut indices = BTreeSet::new();

                            for room_id in room_ids {
                                if let Some(index) = current_values.iter().position(|room| room.room_id() == room_id) {
                                    current_values[index].refresh_cached_data();
                                    indices.insert(index);
                                }
                            }

                            indices.extend(backfill_timestamps(&mut current_values));

                            let diffs = indices
                                .into_iter()
                                .map(|index| VectorDiff::Set { index, value: current_values[index].clone() })
                                .collect::<Vec<_>>();

                            if !diffs.is_empty() {
                                yield diffs;
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

/// Back-fill [`RoomListItem::cached_backfilled_timestamp`] for every room in
/// `rooms`, and return the indices of the rooms where it changed.
///
/// A room without a latest event timestamp (e.g. right after login, before its
/// latest event is computed) would otherwise be sorted after every room having
/// one. Instead, the rooms are walked by recency stamp, from the most recent
/// one, and a room without a timestamp borrows the smallest timestamp among the
/// rooms above it. Rooms above every room having a timestamp borrow the first
/// one.
///
/// The _smallest_ timestamp is used because recency stamps and timestamps don't
/// always agree (e.g. a room with a fresh local echo but an old recency stamp):
/// borrowing from the nearest room alone could move a room above another one
/// the server considers more recent.
///
/// Only rooms having a recency stamp take part: a room without one gets no
/// back-filled timestamp, and doesn't lend its own.
fn backfill_timestamps(rooms: &mut Vector<RoomListItem>) -> Vec<usize> {
    // From the most recent recency stamp to the least recent one. On equal
    // recency stamps, rooms having a timestamp come first, so that the others
    // can borrow it. Rooms without a recency stamp come last.
    let mut indices = (0..rooms.len()).collect::<Vec<_>>();
    indices.sort_by_key(|&index| {
        let room = &rooms[index];
        Reverse((room.cached_recency_stamp, room.cached_latest_event_timestamp.is_some()))
    });

    let mut borrowed_timestamp = indices.iter().find_map(|&index| {
        let room = &rooms[index];
        room.cached_recency_stamp.and(room.cached_latest_event_timestamp)
    });
    let mut changed = Vec::new();

    for index in indices {
        let room = &mut rooms[index];

        let backfilled_timestamp =
            match (room.cached_latest_event_timestamp, room.cached_recency_stamp) {
                (Some(timestamp), Some(_)) => {
                    borrowed_timestamp = Some(
                        borrowed_timestamp.map_or(timestamp, |borrowed| borrowed.min(timestamp)),
                    );
                    None
                }
                (None, Some(_)) => borrowed_timestamp,
                _ => None,
            };

        if room.cached_backfilled_timestamp != backfilled_timestamp {
            room.cached_backfilled_timestamp = backfilled_timestamp;
            changed.push(index);
        }
    }

    changed.sort_unstable();
    changed
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
    /// The [`RoomList`] has not been loaded yet, i.e. a sync might run or not
    /// run at all, there is nothing to show in this `RoomList` yet. It's a good
    /// opportunity to show a placeholder to the user.
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
        /// might have been synced or not synced yet, but we know for sure (from
        /// the server), that there will be this amount of rooms in the list at
        /// the end.
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
            // With this logic, it is possible that `limit` becomes greater than
            // `max` if `max - limit < page_size`, and that's perfectly fine.
            // It's OK to have a `limit` greater than `max`, but it's not OK to
            // increase the limit indefinitely.
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

    /// Timestamp borrowed from other rooms when this room has no
    /// `cached_latest_event_timestamp`, see `backfill_timestamps` (private
    /// function). Not touched by [`Self::refresh_cached_data`].
    pub(super) cached_backfilled_timestamp: Option<MilliSecondsSinceUnixEpoch>,

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
    #[cfg(test)]
    pub(super) fn inner(&self) -> &Room {
        &self.inner
    }

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
            cached_backfilled_timestamp: None,
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

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::async_test;
    use ruma::{UInt, room_id};

    use super::{super::filters::new_rooms, *};

    /// Create rooms with the given `(timestamp, recency stamp)`.
    async fn rooms<const N: usize>(
        stamps: [(Option<u32>, Option<u64>); N],
    ) -> Vector<RoomListItem> {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_ids = [
            room_id!("!0:x.y"),
            room_id!("!1:x.y"),
            room_id!("!2:x.y"),
            room_id!("!3:x.y"),
            room_id!("!4:x.y"),
        ];
        let mut rooms =
            new_rooms(std::array::from_fn::<_, N, _>(|i| room_ids[i]), &client, &server).await;

        for (room, (timestamp, recency_stamp)) in rooms.iter_mut().zip(stamps) {
            room.cached_latest_event_timestamp =
                timestamp.map(|ts| MilliSecondsSinceUnixEpoch(UInt::from(ts)));
            room.cached_recency_stamp = recency_stamp.map(Into::into);
        }

        rooms.into_iter().collect()
    }

    fn backfilled(rooms: &Vector<RoomListItem>) -> Vec<Option<u64>> {
        rooms
            .iter()
            .map(|room| room.cached_backfilled_timestamp.map(|ts| ts.get().into()))
            .collect()
    }

    #[async_test]
    async fn test_backfill_timestamps() {
        let mut rooms = rooms([
            (None, Some(40)), // above every timestamp: borrows the first one
            (Some(300), Some(30)),
            (None, Some(25)), // smallest timestamp above is `!1`'s
            (Some(200), Some(20)),
            (None, Some(10)), // smallest timestamp above is `!3`'s
        ])
        .await;

        assert_eq!(backfill_timestamps(&mut rooms), vec![0, 2, 4]);
        assert_eq!(backfilled(&rooms), vec![Some(300), None, Some(300), None, Some(200)]);

        // Nothing changed: nothing to report.
        assert!(backfill_timestamps(&mut rooms).is_empty());

        // `!4` gets its own timestamp: its back-filled one is cleared.
        rooms[4].cached_latest_event_timestamp =
            Some(MilliSecondsSinceUnixEpoch(UInt::from(100u32)));

        assert_eq!(backfill_timestamps(&mut rooms), vec![4]);
        assert_eq!(backfilled(&rooms), vec![Some(300), None, Some(300), None, None]);
    }

    /// A room with a fresh timestamp but an old recency stamp must not lift the
    /// rooms below it above a room the server considers more recent.
    #[async_test]
    async fn test_backfill_timestamps_takes_the_smallest_timestamp() {
        let mut rooms = rooms([
            (Some(500), Some(100)), // recent room
            (Some(1000), Some(5)),  // fresh timestamp, old recency stamp
            (None, Some(4)),
        ])
        .await;

        backfill_timestamps(&mut rooms);

        assert_eq!(backfilled(&rooms), vec![None, None, Some(500)]);
    }

    #[async_test]
    async fn test_backfill_timestamps_needs_recency_stamps() {
        let mut rooms = rooms([(None, Some(2)), (Some(100), None), (None, None)]).await;

        assert!(backfill_timestamps(&mut rooms).is_empty());
        assert_eq!(backfilled(&rooms), vec![None, None, None]);
    }
}
