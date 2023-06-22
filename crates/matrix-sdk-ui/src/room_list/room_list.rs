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

use std::future::ready;

use eyeball::{shared::Observable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::{RoomListEntry, SlidingSync, SlidingSyncList};
use tokio::{spawn, task::JoinHandle};

use super::{Error, State};

/// A `RoomList` represents a list of rooms, from a
/// [`RoomListService`](super::RoomListService).
#[derive(Debug)]
pub struct RoomList {
    sliding_sync_list: SlidingSyncList,
    loading_state: Observable<RoomListLoadingState>,
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
        let loading_state = Observable::new(RoomListLoadingState::NotLoaded);

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
                        SettingUp | Running => break,
                    }
                }

                // Let's jump from `NotLoaded` to `Loaded`.
                let maximum_number_of_rooms = sliding_sync_list.maximum_number_of_rooms();

                loading_state.set(RoomListLoadingState::Loaded { maximum_number_of_rooms });

                // Wait for updates on the maximum number of rooms to update again.
                while let Some(maximum_number_of_rooms) =
                    sliding_sync_list.maximum_number_of_rooms_stream().next().await
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
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>) {
        self.sliding_sync_list.room_list_stream()
    }

    /// Similar to [`Self::entries`] except that it's possible to provide a
    /// filter that will filter out room list entries.
    pub fn entries_filtered<F>(
        &self,
        filter: F,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>)
    where
        F: Fn(&RoomListEntry) -> bool + Send + Sync + 'static,
    {
        self.sliding_sync_list.room_list_filtered_stream(filter)
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
        /// It does not mean that there is exactly this amount of rooms to
        /// display. Usually, the room entries are represented by
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
