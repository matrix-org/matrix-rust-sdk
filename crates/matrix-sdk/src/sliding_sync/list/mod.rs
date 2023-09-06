mod builder;
mod frozen;
mod request_generator;
mod room_list_entry;
mod sticky;

use std::{
    collections::HashSet,
    fmt::Debug,
    iter,
    ops::RangeInclusive,
    sync::{Arc, RwLock as StdRwLock},
};

use eyeball::Observable;
use eyeball_im::{ObservableVector, VectorDiff};
use eyeball_im_util::vector;
use futures_core::Stream;
use imbl::Vector;
use ruma::{api::client::sync::sync_events::v4, assign, OwnedRoomId, TransactionId};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Sender;
use tracing::{instrument, warn};

use self::sticky::SlidingSyncListStickyParameters;
pub use self::{builder::*, room_list_entry::RoomListEntry};
pub(super) use self::{frozen::FrozenSlidingSyncList, request_generator::*};
use super::{
    sticky_parameters::{LazyTransactionId, SlidingSyncStickyManager},
    Error, SlidingSyncInternalMessage,
};
use crate::Result;

/// Should this [`SlidingSyncList`] be stored in the cache, and automatically
/// reloaded from the cache upon creation?
#[derive(Clone, Copy, Debug)]
pub(crate) enum SlidingSyncListCachePolicy {
    /// Store and load this list from the cache.
    Enabled,
    /// Don't store and load this list from the cache.
    Disabled,
}

/// The type used to express natural bounds (including but not limited to:
/// ranges, timeline limit) in the Sliding Sync.
pub type Bound = u32;

/// One range of rooms in a response from Sliding Sync.
pub type Range = RangeInclusive<Bound>;

/// Many ranges of rooms.
pub type Ranges = Vec<Range>;

/// Holding a specific filtered list within the concept of sliding sync.
///
/// It is OK to clone this type as much as you need: cloning it is cheap.
#[derive(Clone, Debug)]
pub struct SlidingSyncList {
    inner: Arc<SlidingSyncListInner>,
}

impl SlidingSyncList {
    /// Create a new [`SlidingSyncListBuilder`] with the given name.
    pub fn builder(name: impl Into<String>) -> SlidingSyncListBuilder {
        SlidingSyncListBuilder::new(name)
    }

    /// Get the name of the list.
    pub fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    /// Change the sync-mode.
    ///
    /// It is sometimes necessary to change the sync-mode of a list on-the-fly.
    ///
    /// This will change the sync-mode but also the request generator. A new
    /// request generator is generated. Since requests are calculated based on
    /// the request generator, changing the sync-mode is equivalent to
    /// “resetting” the list. It's actually not calling `Self::reset`, which
    /// means that the state is not reset **purposely**. The ranges and the
    /// state will be updated when the next request will be sent and a
    /// response will be received. The maximum number of rooms won't change.
    pub fn set_sync_mode<M>(&self, sync_mode: M)
    where
        M: Into<SlidingSyncMode>,
    {
        self.inner.set_sync_mode(sync_mode.into());

        // When the sync mode is changed, the sync loop must skip over any work in its
        // iteration and jump to the next iteration.
        self.inner.internal_channel_send_if_possible(
            SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
        );
    }

    /// Get the current state.
    pub fn state(&self) -> SlidingSyncListLoadingState {
        self.inner.state.read().unwrap().clone()
    }

    /// Get a stream of state updates.
    ///
    /// If this list has been reloaded from a cache, the initial value read from
    /// the cache will be published.
    ///
    /// There's no guarantee of ordering between items emitted by this stream
    /// and those emitted by other streams exposed on this structure.
    ///
    /// The first part of the returned tuple is the actual loading state, and
    /// the second part is the `Stream` to receive updates.
    pub fn state_stream(
        &self,
    ) -> (SlidingSyncListLoadingState, impl Stream<Item = SlidingSyncListLoadingState>) {
        let read_lock = self.inner.state.read().unwrap();
        let previous_value = (*read_lock).clone();
        let subscriber = Observable::subscribe(&read_lock);

        (previous_value, subscriber)
    }

    /// Get the timeline limit.
    pub fn timeline_limit(&self) -> Option<Bound> {
        self.inner.sticky.read().unwrap().data().timeline_limit()
    }

    /// Set timeline limit.
    pub fn set_timeline_limit(&self, timeline: Option<Bound>) {
        self.inner.sticky.write().unwrap().data_mut().set_timeline_limit(timeline);
    }

    /// Get the current room list.
    pub fn room_list<R>(&self) -> Vec<R>
    where
        R: for<'a> From<&'a RoomListEntry>,
    {
        self.inner.room_list.read().unwrap().iter().map(|e| R::from(e)).collect()
    }

    /// Get a stream of room list.
    ///
    /// If this list has been reloaded from a cache, the initial value read from
    /// the cache will be published.
    ///
    /// There's no guarantee of ordering between items emitted by this stream
    /// and those emitted by other streams exposed on this structure.
    ///
    /// The first part of the returned tuple is the actual room list entries,
    /// and the second part is the `Stream` to receive updates on the room list
    /// entries.
    pub fn room_list_stream(
        &self,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>) {
        let read_lock = self.inner.room_list.read().unwrap();
        let values = (*read_lock).clone();
        let subscriber = ObservableVector::subscribe(&read_lock);

        (values, subscriber.into_stream())
    }

    /// Get a stream of room list, but filtered.
    ///
    /// It's similar to [`Self::room_list_stream`] but the room list is filtered
    /// by `filter`.
    pub fn room_list_filtered_stream<F>(
        &self,
        filter: F,
    ) -> (Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>)
    where
        F: Fn(&RoomListEntry) -> bool,
    {
        let read_lock = self.inner.room_list.read().unwrap();
        let values = (*read_lock).clone();
        let subscriber = ObservableVector::subscribe(&read_lock);

        vector::Filter::new(values, subscriber.into_stream(), filter)
    }

    /// Get the maximum number of rooms. See [`Self::maximum_number_of_rooms`]
    /// to learn more.
    pub fn maximum_number_of_rooms(&self) -> Option<u32> {
        **self.inner.maximum_number_of_rooms.read().unwrap()
    }

    /// Get a stream of rooms count.
    ///
    /// If this list has been reloaded from a cache, the initial value is
    /// published too.
    ///
    /// There's no guarantee of ordering between items emitted by this stream
    /// and those emitted by other streams exposed on this structure.
    pub fn maximum_number_of_rooms_stream(&self) -> impl Stream<Item = Option<u32>> {
        Observable::subscribe(&self.inner.maximum_number_of_rooms.read().unwrap())
    }

    /// Return the `room_id` at the given index.
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.inner
            .room_list
            .read()
            .unwrap()
            .get(index)
            .and_then(|room_list_entry| room_list_entry.as_room_id().map(ToOwned::to_owned))
    }

    /// Calculate the next request and return it.
    ///
    /// The next request is entirely calculated based on the request generator
    /// ([`SlidingSyncListRequestGenerator`]).
    pub(super) fn next_request(
        &self,
        txn_id: &mut LazyTransactionId,
    ) -> Result<v4::SyncRequestList, Error> {
        self.inner.next_request(txn_id)
    }

    /// Returns the current cache policy for this list.
    pub(super) fn cache_policy(&self) -> SlidingSyncListCachePolicy {
        self.inner.cache_policy
    }

    /// Update the list based on the response from the server.
    ///
    /// The `maximum_number_of_rooms` is the `lists.$this_list.count` value,
    /// i.e. maximum number of available rooms as defined by the server. The
    /// `list_sync_operations` is the `list.$this_list.ops` value
    /// received from the server for this specific list. It consists of
    /// operations to “move” rooms' positions. Finally,
    /// the `rooms_that_have_received_an_update` is the `rooms` value received
    /// from the server, which represents aggregated rooms that have received an
    /// update. Maybe their position has changed, maybe they have received a new
    /// event in their timeline. We need this information to update the
    /// `room_list` even if the position of the room hasn't be modified: it
    /// helps the user to know that a room has received an update.
    #[instrument(skip(self, list_sync_operations), fields(name = self.name(), list_sync_operations_count = list_sync_operations.len()))]
    pub(super) fn update(
        &mut self,
        maximum_number_of_rooms: u32,
        list_sync_operations: &[v4::SyncOp],
        rooms_that_have_received_an_update: &[OwnedRoomId],
    ) -> Result<bool, Error> {
        // Make sure to update the generator state first; ordering matters because
        // `update_room_list` observes the latest ranges in the response.
        self.inner.update_request_generator_state(maximum_number_of_rooms)?;

        let new_changes = self.inner.update_room_list(
            maximum_number_of_rooms,
            list_sync_operations,
            rooms_that_have_received_an_update,
        )?;

        Ok(new_changes)
    }

    /// Commit the set of sticky parameters for this list.
    pub fn maybe_commit_sticky(&mut self, txn_id: &TransactionId) {
        self.inner.sticky.write().unwrap().maybe_commit(txn_id);
    }

    /// Manually invalidate the sticky data, so the sticky parameters are
    /// re-sent next time.
    pub fn invalidate_sticky_data(&self) {
        let _ = self.inner.sticky.write().unwrap().data_mut();
    }
}

#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
impl SlidingSyncList {
    /// Set the maximum number of rooms.
    pub(super) fn set_maximum_number_of_rooms(&self, maximum_number_of_rooms: Option<u32>) {
        Observable::set(
            &mut self.inner.maximum_number_of_rooms.write().unwrap(),
            maximum_number_of_rooms,
        );
    }

    /// Get the sync-mode.
    pub fn sync_mode(&self) -> SlidingSyncMode {
        self.inner.sync_mode.read().unwrap().clone()
    }
}

#[derive(Debug)]
pub(super) struct SlidingSyncListInner {
    /// Name of this list to easily recognize them.
    name: String,

    /// The state this list is in.
    state: StdRwLock<Observable<SlidingSyncListLoadingState>>,

    /// Parameters that are sticky, and can be sent only once per session (until
    /// the connection is dropped or the server invalidates what the client
    /// knows).
    sticky: StdRwLock<SlidingSyncStickyManager<SlidingSyncListStickyParameters>>,

    /// The total number of rooms that is possible to interact with for the
    /// given list.
    ///
    /// It's not the total rooms that have been fetched. The server tells the
    /// client that it's possible to fetch this amount of rooms maximum.
    /// Since this number can change according to the list filters, it's
    /// observable.
    maximum_number_of_rooms: StdRwLock<Observable<Option<u32>>>,

    /// The rooms in order.
    room_list: StdRwLock<ObservableVector<RoomListEntry>>,

    /// The request generator, i.e. a type that yields the appropriate list
    /// request. See [`SlidingSyncListRequestGenerator`] to learn more.
    request_generator: StdRwLock<SlidingSyncListRequestGenerator>,

    /// Cache policy for this list.
    cache_policy: SlidingSyncListCachePolicy,

    /// The Sliding Sync internal channel sender. See
    /// [`SlidingSyncInner::internal_channel`] to learn more.
    sliding_sync_internal_channel_sender: Sender<SlidingSyncInternalMessage>,

    #[cfg(any(test, feature = "testing"))]
    sync_mode: StdRwLock<SlidingSyncMode>,
}

impl SlidingSyncListInner {
    /// Change the sync-mode.
    ///
    /// This will change the sync-mode but also the request generator.
    ///
    /// The [`Self::state`] is immediately updated to reflect the new state. The
    /// [`Self::maximum_number_of_rooms`] won't change.
    pub fn set_sync_mode(&self, sync_mode: SlidingSyncMode) {
        #[cfg(any(test, feature = "testing"))]
        {
            *self.sync_mode.write().unwrap() = sync_mode.clone();
        }

        {
            let mut request_generator = self.request_generator.write().unwrap();
            *request_generator = SlidingSyncListRequestGenerator::new(sync_mode);
        }

        {
            let mut state = self.state.write().unwrap();

            let next_state = match **state {
                SlidingSyncListLoadingState::NotLoaded => SlidingSyncListLoadingState::NotLoaded,
                SlidingSyncListLoadingState::Preloaded => SlidingSyncListLoadingState::Preloaded,
                SlidingSyncListLoadingState::PartiallyLoaded
                | SlidingSyncListLoadingState::FullyLoaded => {
                    SlidingSyncListLoadingState::PartiallyLoaded
                }
            };

            Observable::set(&mut state, next_state);
        }
    }

    /// Update the state to the next request, and return it.
    fn next_request(&self, txn_id: &mut LazyTransactionId) -> Result<v4::SyncRequestList, Error> {
        let ranges = {
            // Use a dedicated scope to ensure the lock is released before continuing.
            let mut request_generator = self.request_generator.write().unwrap();
            request_generator
                .generate_next_ranges(**self.maximum_number_of_rooms.read().unwrap())?
        };

        // Here we go.
        Ok(self.request(ranges, txn_id))
    }

    /// Build a [`SyncRequestList`][v4::SyncRequestList] based on the current
    /// state of the request generator.
    #[instrument(skip(self), fields(name = self.name))]
    fn request(&self, ranges: Ranges, txn_id: &mut LazyTransactionId) -> v4::SyncRequestList {
        use ruma::UInt;

        let ranges =
            ranges.into_iter().map(|r| (UInt::from(*r.start()), UInt::from(*r.end()))).collect();

        let mut request = assign!(v4::SyncRequestList::default(), { ranges });
        {
            let mut sticky = self.sticky.write().unwrap();
            sticky.maybe_apply(&mut request, txn_id);
        }

        request
    }

    /// Update the [`Self::room_list`]. It also updates
    /// `[Self::maximum_number_of_rooms]`.
    ///
    /// The `maximum_number_of_rooms` is the `lists.$this_list.count` value,
    /// i.e. maximum number of available rooms as defined by the server. The
    /// `list_sync_operations` is the `list.$this_list.ops` value
    /// received from the server for this specific list. It consists of
    /// operations to “move” rooms' positions. Finally,
    /// the `rooms_that_have_received_an_update` is the `rooms` value received
    /// from the server, which represents aggregated rooms that have received an
    /// update. Maybe their position has changed, maybe they have received a new
    /// event in their timeline. We need this information to update the
    /// `room_list` even if the position of the room hasn't be modified: it
    /// helps the user to know that a room has received an update.
    fn update_room_list(
        &self,
        maximum_number_of_rooms: u32,
        list_sync_operations: &[v4::SyncOp],
        rooms_that_have_received_an_update: &[OwnedRoomId],
    ) -> Result<bool, Error> {
        let mut new_changes = false;

        // Adjust room list entries.
        {
            let number_of_missing_rooms = (maximum_number_of_rooms as usize)
                .saturating_sub(self.room_list.read().unwrap().len());

            if number_of_missing_rooms > 0 {
                self.room_list.write().unwrap().append(
                    iter::repeat(RoomListEntry::Empty).take(number_of_missing_rooms).collect(),
                );

                new_changes = true;
            }
        }

        // Update the `maximum_number_of_rooms` if it has changed.
        {
            let mut maximum_number_of_rooms_lock = self.maximum_number_of_rooms.write().unwrap();

            if Observable::get(&maximum_number_of_rooms_lock) != &Some(maximum_number_of_rooms) {
                Observable::set(&mut maximum_number_of_rooms_lock, Some(maximum_number_of_rooms));

                new_changes = true;
            }
        }

        {
            let mut room_list = self.room_list.write().unwrap();

            // Run the sync operations.
            let mut rooms_that_have_received_an_update =
                HashSet::from_iter(rooms_that_have_received_an_update.iter().cloned());

            if !list_sync_operations.is_empty() {
                apply_sync_operations(
                    list_sync_operations,
                    &mut room_list,
                    &mut rooms_that_have_received_an_update,
                )?;

                new_changes = true;
            }

            // Finally, some rooms have received an update, but their position
            // didn't change in the `room_list`, so no “diff” has
            // been triggered. The `room_list` value is used by the
            // user to know if a room has received an update: be either a
            // position modification or an update in general (like a new room
            // message). Let's trigger those.
            let mut rooms_to_update = Vec::with_capacity(rooms_that_have_received_an_update.len());

            for (position, room_list_entry) in room_list.iter().enumerate() {
                // Invalidated rooms must be considered as empty rooms, so let's just filter by
                // filled rooms.
                if let RoomListEntry::Filled(room_id) = room_list_entry {
                    // If room has received an update but that has not been handled by a
                    // sync operation.
                    if rooms_that_have_received_an_update.contains(room_id) {
                        rooms_to_update.push((position, room_list_entry.clone()));
                    }
                }
            }

            if !rooms_to_update.is_empty() {
                for (position, room_list_entry) in rooms_to_update {
                    // Setting to `room_list`'s item to the same value, just
                    // to generate an “diff update”.
                    room_list.set(position, room_list_entry);
                }

                new_changes = true;
            }
        }

        Ok(new_changes)
    }

    /// Update the state of the [`SlidingSyncListRequestGenerator`] after
    /// receiving a response.
    fn update_request_generator_state(&self, maximum_number_of_rooms: u32) -> Result<(), Error> {
        let mut request_generator = self.request_generator.write().unwrap();
        let new_state = request_generator.handle_response(&self.name, maximum_number_of_rooms)?;
        Observable::set_if_not_eq(&mut self.state.write().unwrap(), new_state);
        Ok(())
    }

    /// Send a message over the internal channel if there is a receiver, i.e. if
    /// the sync loop is running.
    #[instrument]
    fn internal_channel_send_if_possible(&self, message: SlidingSyncInternalMessage) {
        // If there is no receiver, the send will fail, but that's OK here.
        let _ = self.sliding_sync_internal_channel_sender.send(message);
    }
}

fn apply_sync_operations(
    operations: &[v4::SyncOp],
    room_list: &mut ObservableVector<RoomListEntry>,
    rooms_that_have_received_an_update: &mut HashSet<OwnedRoomId>,
) -> Result<(), Error> {
    for operation in operations {
        match &operation.op {
            // Specification says:
            //
            // > Sets a range of entries. Clients SHOULD discard what they previous
            // > knew about entries in this range.
            v4::SlidingOp::Sync => {
                // Extract `start` and `end` from the operation's range.
                let (start, end) = operation
                    .range
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`range` must be present for `SYNC` operation".to_owned(),
                        )
                    })
                    .map(|(start, end)| {
                        (
                            usize::try_from(start).unwrap(),
                            usize::try_from(end).unwrap().saturating_add(1),
                        )
                    })?;

                // Range is invalid.
                if start > end {
                    return Err(Error::BadResponse(format!(
                        "`range` bounds are invalid ({} > {})",
                        start, end,
                    )));
                }

                // Range is too big.
                if end > room_list.len() {
                    return Err(Error::BadResponse(format!(
                        "`range` is out of the `rooms_list`'s bounds ({} > {})",
                        end,
                        room_list.len(),
                    )));
                }

                let room_entry_range = start..end;

                // `room_ids` is absent.
                if operation.room_ids.is_empty() {
                    return Err(Error::BadResponse(
                        "`room_ids` must be present for `SYNC` operation".to_owned(),
                    ));
                }

                let room_ids = operation.room_ids.iter();

                // Mismatch between the `range` and `room_ids`.
                if room_entry_range.len() != room_ids.len() {
                    return Err(Error::BadResponse(
                        format!(
                            "There is a mismatch between the number of items in `range` and `room_ids` ({} != {})",
                            room_entry_range.len(),
                            room_ids.len(),
                        )
                    ));
                }

                // Update parts `room_list`.
                //
                // The room entry index is given by the `room_entry_range` bounds.
                // The room ID is given by the `room_ids`.
                for (room_entry_index, room_id) in room_entry_range.zip(room_ids) {
                    // Syncing means updating the room list to `Filled`.
                    room_list.set(room_entry_index, RoomListEntry::Filled(room_id.clone()));

                    // This `room_id` has been handled, let's remove it from the rooms to handle
                    // later.
                    rooms_that_have_received_an_update.remove(room_id);
                }
            }

            // Specification says:
            //
            // > Remove a single entry. Often comes before an INSERT to allow entries
            // > to move places.
            v4::SlidingOp::Delete => {
                let index = operation
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for `DELETE` operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .unwrap();

                // Index is out of bounds.
                if index >= room_list.len() {
                    // OK, so, normally, we should raise an error. But the server sometimes sends a
                    // `DELETE` for an index that doesn't exist. It happens with the existing
                    // Sliding Sync Proxy (at the time of writing). It may be a bug or something
                    // else. Anyway, it's better to consider an out-of-bounds `DELETE` as a no-op.
                    continue;
                }

                // Removing the entry in the room list.
                let room_entry = room_list.remove(index);

                // This `room_id` has been handled, let's remove it from the rooms to handle
                // later.
                if let Some(room_id) = room_entry.as_room_id() {
                    rooms_that_have_received_an_update.remove(room_id);
                }
            }

            // Specification says:
            //
            // > Sets a single entry. If the position is not empty then clients MUST
            // > move entries to the left or the right depending on where the closest
            // > empty space is.
            v4::SlidingOp::Insert => {
                let index: usize = operation
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for `INSERT` operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .unwrap();
                let room_id = operation.room_id.clone().ok_or_else(|| {
                    Error::BadResponse(
                        "`room_id` must be present for `INSERT` operation".to_owned(),
                    )
                })?;

                // Index is out of bounds.
                if index > room_list.len() {
                    return Err(Error::BadResponse(format!(
                        "`index` is out of the `room_list`' bounds ({} > {})",
                        index,
                        room_list.len(),
                    )));
                }

                // This `room_id` is being handled, let's remove it from the rooms to handle
                // later.
                rooms_that_have_received_an_update.remove(&room_id);

                // Inserting a `Filled` entry in the room list .
                room_list.insert(index, RoomListEntry::Filled(room_id));
            }

            // Specification says:
            //
            // > Remove a range of entries. Clients MAY persist the invalidated
            // > range for offline support, but they should be treated as empty
            // > when additional operations which concern indexes in the range
            // > arrive from the server.
            v4::SlidingOp::Invalidate => {
                // Extract `start` and `end` from the operation's range.
                let (start, end) = operation
                    .range
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`range` must be present for `INVALIDATE` operation".to_owned(),
                        )
                    })
                    .map(|(start, end)| {
                        (
                            usize::try_from(start).unwrap(),
                            usize::try_from(end).unwrap().saturating_add(1),
                        )
                    })?;

                // Range is invalid.
                if start > end {
                    return Err(Error::BadResponse(format!(
                        "`range` bounds are invalid ({} > {})",
                        start, end,
                    )));
                }

                // Range is too big.
                if end > room_list.len() {
                    return Err(Error::BadResponse(format!(
                        "`range` is out of the `room_list`' bounds ({} > {})",
                        end,
                        room_list.len(),
                    )));
                }

                let room_entry_range = start..end;

                // Invalidate parts of `room_list`.
                //
                // The room entry index is given by the `room_entry_range` bounds.
                for room_entry_index in room_entry_range {
                    // Invalidating means updating the room list to `Invalidate`.
                    //
                    // If the previous room list entry is `Filled`, it becomes `Invalidated`.
                    // Otherwise, for `Empty` or `Invalidated`, it stays as is.
                    match room_list.get(room_entry_index) {
                        Some(RoomListEntry::Filled(room_id)) => {
                            // This `room_id` is being handled, let's remove it from the rooms to
                            // handle later.
                            rooms_that_have_received_an_update.remove(room_id);

                            room_list.set(
                                room_entry_index,
                                RoomListEntry::Invalidated(room_id.to_owned()),
                            );
                        }

                        Some(RoomListEntry::Invalidated(room_id)) => {
                            // This `room_id` has been handled, let's remove it from the rooms to
                            // handle later.
                            rooms_that_have_received_an_update.remove(room_id);
                        }

                        _ => {}
                    }
                }
            }

            unknown_operation => {
                warn!("Unknown operation occurred: {unknown_operation:?}");
            }
        }
    }

    Ok(())
}

/// The state the [`SlidingSyncList`] is in.
///
/// The lifetime of a `SlidingSyncList` usually starts at `NotLoaded` or
/// `Preloaded` (if it is restored from a cache). When loading rooms in a list,
/// depending of the [`SlidingSyncMode`], it moves to `PartiallyLoaded` or
/// `FullyLoaded`.
///
/// If the client has been offline for a while, though, the `SlidingSyncList`
/// might return back to `PartiallyLoaded` at any point.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncListLoadingState {
    /// Sliding Sync has not started to load anything yet.
    #[default]
    NotLoaded,
    /// Sliding Sync has been preloaded, i.e. restored from a cache for example.
    Preloaded,
    /// Updates are received from the loaded rooms, and new rooms are being
    /// fetched in the background.
    PartiallyLoaded,
    /// Updates are received for all the loaded rooms, and all rooms have been
    /// loaded!
    FullyLoaded,
}

/// Builder for a new sliding sync list in selective mode.
///
/// Conveniently allows to add ranges.
#[derive(Clone, Debug, Default)]
pub struct SlidingSyncSelectiveModeBuilder {
    ranges: Ranges,
}

impl SlidingSyncSelectiveModeBuilder {
    /// Create a new `SlidingSyncSelectiveModeBuilder`.
    fn new() -> Self {
        Self::default()
    }

    /// Select a range to fetch.
    pub fn add_range(mut self, range: Range) -> Self {
        self.ranges.push(range);
        self
    }

    /// Select many ranges to fetch.
    pub fn add_ranges(mut self, ranges: Ranges) -> Self {
        self.ranges.extend(ranges);
        self
    }
}

impl From<SlidingSyncSelectiveModeBuilder> for SlidingSyncMode {
    fn from(builder: SlidingSyncSelectiveModeBuilder) -> Self {
        Self::Selective { ranges: builder.ranges }
    }
}

#[derive(Clone, Debug)]
enum WindowedModeBuilderKind {
    Paging,
    Growing,
}

/// Builder for a new sliding sync list in growing/paging mode.
#[derive(Clone, Debug)]
pub struct SlidingSyncWindowedModeBuilder {
    mode: WindowedModeBuilderKind,
    batch_size: u32,
    maximum_number_of_rooms_to_fetch: Option<u32>,
}

impl SlidingSyncWindowedModeBuilder {
    fn new(mode: WindowedModeBuilderKind, batch_size: u32) -> Self {
        Self { mode, batch_size, maximum_number_of_rooms_to_fetch: None }
    }

    /// The maximum number of rooms to fetch.
    pub fn maximum_number_of_rooms_to_fetch(mut self, num: u32) -> Self {
        self.maximum_number_of_rooms_to_fetch = Some(num);
        self
    }
}

impl From<SlidingSyncWindowedModeBuilder> for SlidingSyncMode {
    fn from(builder: SlidingSyncWindowedModeBuilder) -> Self {
        match builder.mode {
            WindowedModeBuilderKind::Paging => Self::Paging {
                batch_size: builder.batch_size,
                maximum_number_of_rooms_to_fetch: builder.maximum_number_of_rooms_to_fetch,
            },
            WindowedModeBuilderKind::Growing => Self::Growing {
                batch_size: builder.batch_size,
                maximum_number_of_rooms_to_fetch: builder.maximum_number_of_rooms_to_fetch,
            },
        }
    }
}

/// How a [`SlidingSyncList`] fetches the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncMode {
    /// Only sync the specific defined windows/ranges.
    Selective {
        /// The specific defined ranges.
        ranges: Ranges,
    },

    /// Fully sync all rooms in the background, page by page of `batch_size`,
    /// like `0..=19`, `20..=39`, 40..=59` etc. assuming the `batch_size` is 20.
    Paging {
        /// The batch size.
        batch_size: u32,

        /// The maximum number of rooms to fetch. `None` to fetch everything
        /// possible.
        maximum_number_of_rooms_to_fetch: Option<u32>,
    },

    /// Fully sync all rooms in the background, with a growing window of
    /// `batch_size`, like `0..=19`, `0..=39`, `0..=59` etc. assuming the
    /// `batch_size` is 20.
    Growing {
        /// The batch size.
        batch_size: u32,

        /// The maximum number of rooms to fetch. `None` to fetch everything
        /// possible.
        maximum_number_of_rooms_to_fetch: Option<u32>,
    },
}

impl Default for SlidingSyncMode {
    fn default() -> Self {
        Self::Selective { ranges: Vec::new() }
    }
}

impl SlidingSyncMode {
    /// Create a `SlidingSyncMode::Selective`.
    pub fn new_selective() -> SlidingSyncSelectiveModeBuilder {
        SlidingSyncSelectiveModeBuilder::new()
    }

    /// Create a `SlidingSyncMode::Paging`.
    pub fn new_paging(batch_size: u32) -> SlidingSyncWindowedModeBuilder {
        SlidingSyncWindowedModeBuilder::new(WindowedModeBuilderKind::Paging, batch_size)
    }

    /// Create a `SlidingSyncMode::Growing`.
    pub fn new_growing(batch_size: u32) -> SlidingSyncWindowedModeBuilder {
        SlidingSyncWindowedModeBuilder::new(WindowedModeBuilderKind::Growing, batch_size)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use eyeball_im::{ObservableVector, VectorDiff};
    use futures_util::StreamExt;
    use imbl::vector;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::sync::sync_events::v4::{self, SlidingOp},
        room_id, uint, OwnedRoomId,
    };
    use serde_json::json;
    use tokio::{
        spawn,
        sync::{
            broadcast::{channel, error::TryRecvError},
            mpsc::unbounded_channel,
        },
    };

    use super::{
        apply_sync_operations, RoomListEntry, SlidingSyncList, SlidingSyncListLoadingState,
        SlidingSyncMode,
    };
    use crate::sliding_sync::{sticky_parameters::LazyTransactionId, SlidingSyncInternalMessage};

    macro_rules! assert_json_roundtrip {
        (from $type:ty: $rust_value:expr => $json_value:expr) => {
            let json = serde_json::to_value(&$rust_value).unwrap();
            assert_eq!(json, $json_value);

            let rust: $type = serde_json::from_value(json).unwrap();
            assert_eq!(rust, $rust_value);
        };
    }

    #[async_test]
    async fn test_sliding_sync_list_selective_mode() {
        let (sender, mut receiver) = channel(1);

        // Set range on `Selective`.
        let list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=1).add_range(2..=3))
            .build(sender);

        {
            let mut generator = list.inner.request_generator.write().unwrap();
            assert_eq!(generator.requested_ranges(), &[0..=1, 2..=3]);

            let ranges = generator.generate_next_ranges(None).unwrap();
            assert_eq!(ranges, &[0..=1, 2..=3]);
        }

        // There shouldn't be any internal request to restart the sync loop yet.
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(4..=5));

        {
            let mut generator = list.inner.request_generator.write().unwrap();
            assert_eq!(generator.requested_ranges(), &[4..=5]);

            let ranges = generator.generate_next_ranges(None).unwrap();
            assert_eq!(ranges, &[4..=5]);
        }

        // Setting the sync mode requests exactly one restart of the sync loop.
        assert!(matches!(
            receiver.try_recv(),
            Ok(SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_sliding_sync_list_timeline_limit() {
        let (sender, _receiver) = channel(1);

        let list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=1))
            .timeline_limit(7)
            .build(sender);

        assert_eq!(list.inner.sticky.read().unwrap().data().timeline_limit(), Some(7));

        list.set_timeline_limit(Some(42));
        assert_eq!(list.inner.sticky.read().unwrap().data().timeline_limit(), Some(42));

        list.set_timeline_limit(None);
        assert_eq!(list.inner.sticky.read().unwrap().data().timeline_limit(), None);
    }

    #[test]
    fn test_sliding_sync_get_room_id() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=1))
            .build(sender);

        let room0 = room_id!("!room0:bar.org");
        let room1 = room_id!("!room1:bar.org");

        // Simulate a request.
        let _ = list.next_request(&mut LazyTransactionId::new());

        // A new response.
        let sync0: v4::SyncOp = serde_json::from_value(json!({
            "op": SlidingOp::Sync,
            "range": [0, 1],
            "room_ids": [room0, room1],
        }))
        .unwrap();

        list.update(6, &[sync0], &[]).unwrap();

        assert_eq!(list.get_room_id(0), Some(room0.to_owned()));
        assert_eq!(list.get_room_id(1), Some(room1.to_owned()));
        assert_eq!(list.get_room_id(2), None);
    }

    macro_rules! assert_ranges {
        (
            list = $list:ident,
            list_state = $first_list_state:ident,
            maximum_number_of_rooms = $maximum_number_of_rooms:expr,
            $(
                next => {
                    ranges = $( $range_start:literal ..= $range_end:literal ),* ,
                    is_fully_loaded = $is_fully_loaded:expr,
                    list_state = $list_state:ident,
                }
            ),*
            $(,)*
        ) => {
            assert_eq!($list.state(), SlidingSyncListLoadingState::$first_list_state, "first state");

            $(
                {
                    // Generate a new request.
                    let request = $list.next_request(&mut LazyTransactionId::new()).unwrap();

                    assert_eq!(
                        request.ranges,
                        [
                            $( (uint!( $range_start ), uint!( $range_end )) ),*
                        ],
                        "ranges",
                    );

                    // Fake a response.
                    let _ = $list.update($maximum_number_of_rooms, &[], &[]);

                    assert_eq!(
                        $list.inner.request_generator.read().unwrap().is_fully_loaded(),
                        $is_fully_loaded,
                        "is fully loaded",
                    );
                    assert_eq!(
                        $list.state(),
                        SlidingSyncListLoadingState::$list_state,
                        "state",
                    );
                }
            )*
        };
    }

    #[test]
    fn test_generator_paging_full_sync() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_paging(10))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 20..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_paging_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_paging(10).maximum_number_of_rooms_to_fetch(22))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms to fetch is reached!
            next => {
                ranges = 20..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=21, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_growing(10))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_growing(10).maximum_number_of_rooms_to_fetch(22))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_selective() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            }
        };
    }

    #[async_test]
    async fn test_generator_selective_with_modifying_ranges_on_the_fly() {
        let (sender, _receiver) = channel(4);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            }
        };

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(3..=7));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 3..=7,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(42..=77));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = 42..=77,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };

        list.set_sync_mode(SlidingSyncMode::new_selective());

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            next => {
                ranges = ,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[async_test]
    async fn test_generator_changing_sync_mode_to_various_modes() {
        let (sender, _receiver) = channel(4);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            }
        };

        // Changing from `Selective` to `Growing`.
        list.set_sync_mode(SlidingSyncMode::new_growing(10));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };

        // Changing from `Growing` to `Paging`.
        list.set_sync_mode(SlidingSyncMode::new_paging(10));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 20..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };

        // Changing from `Paging` to `Selective`.
        list.set_sync_mode(SlidingSyncMode::new_selective());

        assert_eq!(list.state(), SlidingSyncListLoadingState::PartiallyLoaded); // we had some partial state, but we can't be sure it's fully loaded until the
                                                                                // next request

        // We need to update the ranges, of course, as they are not managed
        // automatically anymore.
        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(0..=100));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
            }
        };
    }

    #[async_test]
    #[allow(clippy::await_holding_lock)]
    async fn test_sliding_sync_inner_update_state_room_list_and_maximum_number_of_rooms() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=3))
            .build(sender);

        assert_eq!(**list.inner.maximum_number_of_rooms.read().unwrap(), None);
        assert_eq!(list.inner.room_list.read().unwrap().len(), 0);

        let room0 = room_id!("!room0:bar.org");
        let room1 = room_id!("!room1:bar.org");
        let room2 = room_id!("!room2:bar.org");
        let room3 = room_id!("!room3:bar.org");
        let room4 = room_id!("!room4:bar.org");

        // Initial range.
        for _ in 0..=1 {
            // Simulate a request.
            let _ = list.next_request(&mut LazyTransactionId::new());

            // A new response.
            let sync: v4::SyncOp = serde_json::from_value(json!({
                "op": SlidingOp::Sync,
                "range": [0, 2],
                "room_ids": [room0, room1, room2],
            }))
            .unwrap();

            let new_changes = list.update(5, &[sync], &[]).unwrap();

            assert!(new_changes);

            // The `maximum_number_of_rooms` has been updated as expected.
            assert_eq!(**list.inner.maximum_number_of_rooms.read().unwrap(), Some(5));

            // The `room_list` has the correct size and contains expected room entries.
            let room_list = list.inner.room_list.read().unwrap();

            assert_eq!(room_list.len(), 5);
            assert_eq!(
                **room_list,
                vector![
                    RoomListEntry::Filled(room0.to_owned()),
                    RoomListEntry::Filled(room1.to_owned()),
                    RoomListEntry::Filled(room2.to_owned()),
                    RoomListEntry::Empty,
                    RoomListEntry::Empty,
                ]
            );
        }

        let (_, mut room_list_stream) = list.room_list_stream();
        let (room_list_stream_sender, mut room_list_stream_receiver) = unbounded_channel();

        spawn(async move {
            while let Some(diff) = room_list_stream.next().await {
                room_list_stream_sender.send(diff).unwrap();
            }
        });

        // Simulate a request.
        let _ = list.next_request(&mut LazyTransactionId::new());

        // A new response.
        let sync: v4::SyncOp = serde_json::from_value(json!({
            "op": SlidingOp::Sync,
            "range": [3, 4],
            "room_ids": [room3, room4],
        }))
        .unwrap();

        let new_changes = list
            .update(
                5,
                &[sync],
                // Let's imagine `room2` has received an update, but its position doesn't
                // change.
                &[room3.to_owned(), room4.to_owned(), room2.to_owned()],
            )
            .unwrap();

        assert!(new_changes);

        // The `maximum_number_of_rooms` has been updated as expected.
        assert_eq!(**list.inner.maximum_number_of_rooms.read().unwrap(), Some(5));

        // The `room_list` has the correct size and contains expected room entries.
        let room_list = list.inner.room_list.read().unwrap();

        assert_eq!(room_list.len(), 5);
        assert_eq!(
            **room_list,
            vector![
                RoomListEntry::Filled(room0.to_owned()),
                RoomListEntry::Filled(room1.to_owned()),
                RoomListEntry::Filled(room2.to_owned()),
                RoomListEntry::Filled(room3.to_owned()),
                RoomListEntry::Filled(room4.to_owned()),
            ]
        );

        // Wait for “diff” to be generated.
        // Why this? Because the following `assert_eq` are using `try_recv()` instead of
        // recv().await`, so that a missing “diff” doesn't make the test to hang
        // forever.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // `room3` has been modified by a `SYNC` operation.
        assert_eq!(
            room_list_stream_receiver.try_recv(),
            Ok(VectorDiff::Set { index: 3, value: RoomListEntry::Filled(room3.to_owned()) })
        );
        // `room4` has been modified by a `SYNC` operation.
        assert_eq!(
            room_list_stream_receiver.try_recv(),
            Ok(VectorDiff::Set { index: 4, value: RoomListEntry::Filled(room4.to_owned()) })
        );
        // `room2` has been modified by another update (like a new event).
        assert_eq!(
            room_list_stream_receiver.try_recv(),
            Ok(VectorDiff::Set { index: 2, value: RoomListEntry::Filled(room2.to_owned()) })
        );
    }

    #[test]
    fn test_sliding_sync_mode_serialization() {
        assert_json_roundtrip!(
            from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_paging(1).maximum_number_of_rooms_to_fetch(2)) => json!({
                "Paging": {
                    "batch_size": 1,
                    "maximum_number_of_rooms_to_fetch": 2
                }
            })
        );
        assert_json_roundtrip!(
            from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_growing(1).maximum_number_of_rooms_to_fetch(2)) => json!({
                "Growing": {
                    "batch_size": 1,
                    "maximum_number_of_rooms_to_fetch": 2
                }
            })
        );
        assert_json_roundtrip!(from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_selective()) => json!({
                "Selective": {
                    "ranges": []
                }
            })
        );
    }

    #[test]
    fn test_sliding_sync_state_serialization() {
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::NotLoaded => json!("NotLoaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::Preloaded => json!("Preloaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::PartiallyLoaded => json!("PartiallyLoaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::FullyLoaded => json!("FullyLoaded"));
    }

    macro_rules! entries {
        ( @_ [ E $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
            entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Empty, ] )
        };

        ( @_ [ F( $room_id:literal ) $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
            entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Filled(room_id!( $room_id ).to_owned()), ] )
        };

        ( @_ [ I( $room_id:literal ) $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
            entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Invalidated(room_id!( $room_id ).to_owned()), ] )
        };

        ( @_ [] [ $( $accumulator:tt )* ] ) => {
            vector![ $( $accumulator )* ]
        };

        ( $( $all:tt )* ) => {
            entries!( @_ [ $( $all )* ] [] )
        };
    }

    macro_rules! assert_sync_operations {
        (
            $assert_description:literal :
            room_list = [ $( $room_list_entries:tt )* ],
            sync_operations = [
                $(
                    { $( $operation:tt )+ }
                ),*
                $(,)?
            ]
            $( , rooms = [ $( $rooms:literal ),* ] )?
            $(,)?
            =>
            result = $result:tt,
            room_list = [ $( $expected_room_list_entries:tt )* ]
            $( , rooms = [ $( $expected_rooms:literal ),* ] )?
            $(,)?
        ) => {
            let mut room_list = ObservableVector::from(entries![ $( $room_list_entries )* ]);
            let operations: &[v4::SyncOp] = &[
                $(
                    serde_json::from_value(json!({
                        $( $operation )+
                    })).unwrap()
                ),*
            ];

            let mut rooms_that_have_received_an_update = HashSet::<OwnedRoomId>::new();

            $(
                {
                    $(
                        rooms_that_have_received_an_update.insert(room_id!( $rooms ).to_owned());
                    )*
                }
            )?

            let result = apply_sync_operations(operations, &mut room_list, &mut rooms_that_have_received_an_update);

            assert!(result.$result(), "{}; assert the `Result`", $assert_description);
            assert_eq!(
                *room_list,
                entries![ $( $expected_room_list_entries )* ],
                "{}; asserting the `room_list`",
                $assert_description,
            );

            $(
                #[allow(unused_mut)]
                let mut expected_rooms_that_have_received_an_update = HashSet::<OwnedRoomId>::new();

                {
                    $(
                        expected_rooms_that_have_received_an_update.insert(room_id!( $expected_rooms ).to_owned());
                    )*
                }

                assert_eq!(
                    rooms_that_have_received_an_update,
                    expected_rooms_that_have_received_an_update,
                    "{}; asserting the rooms that have received an update",
                    $assert_description,
                );
            )?
        };
    }

    #[test]
    fn test_sync_operations_sync() {
        assert_sync_operations! {
            "All room list is updated":
            room_list = [E, E, E, F("!r3:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 2],
                    "room_ids": ["!r0:x.y", "!r1:x.y", "!r2:x.y"],
                }
            ],
            rooms = ["!r0:x.y", "!r1:x.y", "!r2:x.y", "!r3:x.y"], // `r3` has received an update, but its position didn't change
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y"), F("!r3:x.y")],
            rooms = ["!r3:x.y"],
        };

        assert_sync_operations! {
            "Partial update":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 1],
                    "room_ids": ["!r0:x.y", "!r1:x.y"],
                }
            ]
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), F("!r1:x.y"), E],
        };

        assert_sync_operations! {
            "Partial update":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [1, 2],
                    "room_ids": ["!r1:x.y", "!r2:x.y"],
                }
            ]
            =>
            result = is_ok,
            room_list = [E, F("!r1:x.y"), F("!r2:x.y")],
        };

        assert_sync_operations! {
            "The range returned by the server is too large compared to the `room_ids`":
            room_list = [E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 2], // <- it should be [0, 0]
                    "room_ids": ["!r0:x.y"],
                }
            ]
            =>
            result = is_err,
            room_list = [E],
        };

        assert_sync_operations! {
            "Missing `range`":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "room_ids": ["!r0:x.y", "!r1:x.y"],
                }
            ]
            =>
            result = is_err,
            room_list = [E, E, E],
        };

        assert_sync_operations! {
            "Invalid `range`":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [2, 0],
                    "room_ids": ["!r0:x.y", "!r1:x.y"],
                }
            ]
            =>
            result = is_err,
            room_list = [E, E, E],
        };

        assert_sync_operations! {
            "Missing `room_ids`":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 2],
                }
            ]
            =>
            result = is_err,
            room_list = [E, E, E],
        };

        assert_sync_operations! {
            "Out of bounds operation":
            room_list = [E, F("!r1:x.y"), E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [2, 3],
                    "room_ids": ["!r1:x.y", "!r2:x.y"],
                }
            ]
            =>
            result = is_err,
            room_list = [E, F("!r1:x.y"), E],
        };

        assert_sync_operations! {
            "The server replies with a particular range, but some room IDs are missing":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 2],
                    "room_ids": ["!r0:x.y" /* items are missing here! */],
                }
            ]
            =>
            result = is_err,
            room_list = [E, E, E],
        };

        assert_sync_operations! {
            "The server replies with a particular range, but there is too much room IDs":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Sync,
                    "range": [0, 1],
                    "room_ids": ["!r0:x.y", "!r1:x.y", "!extra:x.y"],
                }
            ]
            =>
            result = is_err,
            room_list = [E, E, E],
        };
    }

    #[test]
    fn test_sync_operations_delete() {
        assert_sync_operations! {
            "Delete a room entry in the middle":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "index": 1,
                }
            ],
            rooms = ["!r0:x.y", "!r1:x.y"], // `r0` has also received an update.
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), F("!r2:x.y")],
            rooms = ["!r0:x.y"],
        };

        assert_sync_operations! {
            "Delete a room entry at the beginning":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "index": 0,
                }
            ]
            =>
            result = is_ok,
            room_list = [F("!r1:x.y"), F("!r2:x.y")],
        };

        assert_sync_operations! {
            "Delete a room entry at the end":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "index": 2,
                }
            ]
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), F("!r1:x.y")],
        };

        assert_sync_operations! {
            "Delete an out of bounds room entry":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "index": 3,
                }
            ]
            =>
            result = is_ok, // <- that's surprising, see the code for more explanations!
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
        };
    }

    #[test]
    fn test_sync_operations_insert() {
        assert_sync_operations! {
            "Insert a room entry in the middle":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Insert,
                    "index": 1,
                    "room_id": "!r1:x.y",
                }
            ],
            rooms = ["!r0:x.y", "!r1:x.y"], // `r0` has also received an update.
            =>
            result = is_ok,
            room_list = [E, F("!r1:x.y"), E, E],
            rooms = ["!r0:x.y"],
        };

        assert_sync_operations! {
            "Insert a room entry at the beginning":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Insert,
                    "index": 0,
                    "room_id": "!r0:x.y",
                }
            ]
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), E, E, E],
        };

        assert_sync_operations! {
            "Insert a room entry at the end":
            room_list = [E, E, E],
            sync_operations = [
                {
                    "op": SlidingOp::Insert,
                    "index": 3,
                    "room_id": "!r3:x.y",
                }
            ]
            =>
            result = is_ok,
            room_list = [E, E, E, F("!r3:x.y")],
        };

        assert_sync_operations! {
            "Insert an out of bounds room entry":
            room_list = [E, F("!r1:x.y"), E],
            sync_operations = [
                {
                    "op": SlidingOp::Insert,
                    "index": 4,
                    "room_id": "!r4:x.y",
                }
            ]
            =>
            result = is_err,
            room_list = [E, F("!r1:x.y"), E],
        };
    }

    #[test]
    fn test_sync_operations_invalidate() {
        assert_sync_operations! {
            "Invalidating an empty room":
            room_list = [E, F("!r1:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 0],
                }
            ],
            rooms = ["!r1:x.y"], // `r1` has also received an update.
            =>
            result = is_ok,
            room_list = [E, F("!r1:x.y")],
            rooms = ["!r1:x.y"],
        };

        assert_sync_operations! {
            "Invalidating a filled room":
            room_list = [F("!r0:x.y"), F("!r1:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 0],
                }
            ],
            rooms = ["!r0:x.y", "!r1:x.y"], // `r1` has also received an update.
            =>
            result = is_ok,
            room_list = [I("!r0:x.y"), F("!r1:x.y")],
            rooms = ["!r1:x.y"],
        };

        assert_sync_operations! {
            "Invalidating an invalidated room":
            room_list = [I("!r0:x.y"), F("!r1:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 0],
                }
            ],
            rooms = ["!r0:x.y", "!r1:x.y"], // `r1` has also received an update.
            =>
            result = is_ok,
            room_list = [I("!r0:x.y"), F("!r1:x.y")],
            rooms = ["!r1:x.y"],
        };

        assert_sync_operations! {
            "Partial update from the beginning":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 1],
                }
            ]
            =>
            result = is_ok,
            room_list = [I("!r0:x.y"), I("!r1:x.y"), F("!r2:x.y")],
        };

        assert_sync_operations! {
            "Partial update from the end":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [1, 2],
                }
            ]
            =>
            result = is_ok,
            room_list = [F("!r0:x.y"), I("!r1:x.y"), I("!r2:x.y")],
        };

        assert_sync_operations! {
            "Full update":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 2],
                }
            ]
            =>
            result = is_ok,
            room_list = [I("!r0:x.y"), I("!r1:x.y"), I("!r2:x.y")],
        };

        assert_sync_operations! {
            "The range returned by the server is too large compared to the `room_lists`":
            room_list = [F("!r0:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Invalidate,
                    "range": [0, 9], // <- it should be [0, 0]
                }
            ]
            =>
            result = is_err,
            room_list = [F("!r0:x.y")],
        };

        assert_sync_operations! {
            "Missing `range`":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                }
            ]
            =>
            result = is_err,
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
        };

        assert_sync_operations! {
            "Invalid `range`":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "range": [12, 0],
                }
            ]
            =>
            result = is_err,
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
        };

        assert_sync_operations! {
            "Out of bounds operation":
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
            sync_operations = [
                {
                    "op": SlidingOp::Delete,
                    "range": [2, 3],
                }
            ]
            =>
            result = is_err,
            room_list = [F("!r0:x.y"), F("!r1:x.y"), F("!r2:x.y")],
        };
    }

    #[test]
    fn test_once_built() {
        let (sender, _receiver) = channel(1);

        let probe = Arc::new(Mutex::new(Cell::new(false)));
        let probe_clone = probe.clone();

        let _list = SlidingSyncList::builder("testing")
            .once_built(move |list| {
                let mut probe_lock = probe.lock().unwrap();
                *probe_lock.get_mut() = true;

                list
            })
            .build(sender);

        let probe_lock = probe_clone.lock().unwrap();
        assert!(probe_lock.get());
    }
}
