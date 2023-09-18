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

use std::{
    future::ready,
    sync::{Arc, Mutex},
};

use async_cell::sync::AsyncCell;
use async_rx::StreamExt as _;
use async_stream::stream;
use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use eyeball_im_util::vector::Limit;
use futures_util::{pin_mut, stream, Stream, StreamExt as _};
use matrix_sdk::{
    executor::{spawn, JoinHandle},
    RoomListEntry, SlidingSync, SlidingSyncList,
};

use super::{Error, State};

/// A `RoomList` represents a list of rooms, from a
/// [`RoomListService`](super::RoomListService).
#[derive(Debug)]
pub struct RoomList {
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
        sliding_sync: &SlidingSync,
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
    pub fn loading_state(&self) -> Subscriber<RoomListLoadingState> {
        self.loading_state.subscribe()
    }

    /// Get all previous room list entries, in addition to a [`Stream`] to room
    /// list entry's updates.
    pub fn entries(
        &self,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = Vec<VectorDiff<RoomListEntry>>>) {
        self.sliding_sync_list.room_list_stream()
    }

    /// Similar to [`Self::entries`] except that it's possible to provide a
    /// filter that will filter out room list entries, and that it's also
    /// possible to “paginate” over the entries by `page_size`.
    ///
    /// The returned stream will only start yielding diffs once a filter is set
    /// through the returned [`RoomListDynamicEntriesController`]. For every
    /// call to [`RoomListDynamicEntriesController::set_filter`], the stream
    /// will yield a [`VectorDiff::Clear`] followed by any updates of the
    /// room list under that filter (until the next reset).
    pub fn entries_with_dynamic_adapters(
        &self,
        page_size: usize,
    ) -> (impl Stream<Item = Vec<VectorDiff<RoomListEntry>>>, RoomListDynamicEntriesController)
    {
        let list = self.sliding_sync_list.clone();

        let filter_fn_cell = AsyncCell::shared();

        let dynamic_limit = SharedObservable::<usize>::new(0);
        let dynamic_limit_stream = dynamic_limit.subscribe();

        let dynamic_entries_controller = RoomListDynamicEntriesController::new(
            filter_fn_cell.clone(),
            page_size,
            dynamic_limit.clone(),
            list.maximum_number_of_rooms_stream(),
        );

        let stream = stream! {
            loop {
                let filter_fn = filter_fn_cell.take().await;
                let (items, stream) = list.room_list_filtered_stream(filter_fn);
                let stream = Limit::dynamic(items, stream, dynamic_limit_stream.clone());

                dynamic_limit.set(page_size);

                // Clearing the stream before chaining with the real stream.
                yield stream::once(ready(vec![VectorDiff::Clear]))
                    .chain(stream);
            }
        }
        .switch();

        (stream, dynamic_entries_controller)
    }
}

/// The loading state of a [`RoomList`].
///
/// When a [`RoomList`] is displayed to the user, it can be in various states.
/// This enum tries to represent those states with a correct level of
/// abstraction.
#[derive(Clone, Debug)]
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
        /// Usually, the room entries are represented by
        /// [`RoomListEntry`]. The room entry might have been synced or not
        /// synced yet, but we know for sure (from the server), that there will
        /// be this amount of rooms in the list at the end.
        ///
        /// Note that it's an `Option`, because it may be possible that the
        /// server did miss to send us this value. It's up to you, dear reader,
        /// to know which default to adopt in case of `None`.
        maximum_number_of_rooms: Option<u32>,
    },
}

type BoxedFilterFn = Box<dyn Fn(&RoomListEntry) -> bool + Send + Sync>;

/// Controller for the [`RoomList`] dynamic entries.
///
/// To get one value of this type, use
/// [`RoomList::entries_with_dynamic_adapters`]
pub struct RoomListDynamicEntriesController {
    filter: Arc<AsyncCell<BoxedFilterFn>>,
    page_size: usize,
    limit: SharedObservable<usize>,
    maximum_number_of_rooms: Mutex<Subscriber<Option<u32>>>,
}

impl RoomListDynamicEntriesController {
    fn new(
        filter: Arc<AsyncCell<BoxedFilterFn>>,
        page_size: usize,
        limit_stream: SharedObservable<usize>,
        maximum_number_of_rooms: Subscriber<Option<u32>>,
    ) -> Self {
        Self {
            filter,
            page_size,
            limit: limit_stream,
            maximum_number_of_rooms: Mutex::new(maximum_number_of_rooms),
        }
    }

    /// Set the filter.
    ///
    /// If the associated stream has been dropped, returns `false` to indicate
    /// the operation didn't have an effect.
    pub fn set_filter(
        &self,
        filter: impl Fn(&RoomListEntry) -> bool + Send + Sync + 'static,
    ) -> bool {
        if Arc::strong_count(&self.filter) == 1 {
            // there is no other reference to the boxed filter fn, setting it
            // would be pointless (no new references can be created from self,
            // either)
            false
        } else {
            self.filter.set(Box::new(filter));
            true
        }
    }

    /// Add one page, i.e. view `page_size` more entries in the room list if
    /// any.
    pub fn add_one_page(&self) {
        let Some(max) = self.maximum_number_of_rooms.lock().unwrap().next_now() else {
            return;
        };

        let max: usize = max.try_into().unwrap();
        let limit = self.limit.get();

        if limit < max {
            // With this logic, it is possible that `limit` becomes greater than `max` if
            // `max - limit < page_size`, and that's perfectly fine. It's OK to have a
            // `limit` greater than `max`, but it's not OK to increase the limit
            // indefinitely.
            self.limit.set(limit + self.page_size);
        }
    }

    /// Reset the one page, i.e. forget all pages and move back to the first
    /// page.
    pub fn reset_to_one_page(&self) {
        self.limit.set(self.page_size);
    }
}
