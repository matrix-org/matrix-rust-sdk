// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::room::{
    ListThreadsOptions as SdkListThreadsOptions, ThreadSubscription as SdkThreadSubscription,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::timeline::{
    RoomExt,
    thread_list_service::{
        ThreadListItem as UIThreadListItem, ThreadListItemEvent as UIThreadListItemEvent,
        ThreadListPaginationState as UIThreadListPaginationState,
        ThreadListService as UIThreadListService,
        ThreadListServiceError as UIThreadListServiceError,
    },
};
use ruma::api::client::threads::get_threads::v1::IncludeThreads as SdkIncludeThreads;

use crate::{
    TaskHandle,
    error::ClientError,
    runtime::get_runtime_handle,
    timeline::{ProfileDetails, TimelineItemContent},
    utils::Timestamp,
};

/// A thread subscription (MSC4306).
#[derive(uniffi::Record)]
pub struct ThreadSubscription {
    /// Whether the thread subscription happened automatically (e.g. after a
    /// mention) or if it was manually requested by the user.
    pub automatic: bool,
}

impl From<ThreadSubscription> for SdkThreadSubscription {
    fn from(subscription: ThreadSubscription) -> Self {
        Self { automatic: subscription.automatic }
    }
}

/// Options for [`Room::load_thread_list`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ListThreadsOptions {
    /// An extra filter to select which threads should be returned.
    pub include_threads: IncludeThreads,

    /// The token to start returning events from.
    ///
    /// This token can be obtained from a [`ThreadList::prev_batch_token`]
    /// returned by a previous call to [`Room::load_thread_list()`].
    ///
    /// If `from` isn't provided the homeserver shall return a list of thread
    /// roots from end of the timeline history.
    pub from: Option<String>,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: Option<u64>,
}

impl From<ListThreadsOptions> for SdkListThreadsOptions {
    fn from(opts: ListThreadsOptions) -> Self {
        Self {
            include_threads: opts.include_threads.into(),
            from: opts.from,
            limit: opts.limit.and_then(ruma::UInt::new),
        }
    }
}

/// Which threads to include in the response.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum IncludeThreads {
    /// `all`
    ///
    /// Include all thread roots found in the room.
    ///
    /// This is the default.
    All,

    /// `participated`
    ///
    /// Only include thread roots for threads where
    /// [`current_user_participated`] is `true`.
    ///
    /// [`current_user_participated`]: https://spec.matrix.org/latest/client-server-api/#server-side-aggregation-of-mthread-relationships
    Participated,
}

impl From<IncludeThreads> for SdkIncludeThreads {
    fn from(include_threads: IncludeThreads) -> Self {
        match include_threads {
            IncludeThreads::All => Self::All,
            IncludeThreads::Participated => Self::Participated,
        }
    }
}

/// Each `ThreadListItem` represents one thread root event in the room. The
/// fields are pre-resolved from the raw homeserver response: the sender's
/// profile is fetched eagerly and the event content is parsed into a
/// `TimelineItemContent` so that consumers can render the item without any
/// additional work.
///
/// `ThreadListItem`s are produced page by page via `Room::load_thread_list()`
/// and are accumulated inside the `ThreadListService` as pages are fetched
/// through `ThreadListService::paginate()`.
#[derive(uniffi::Record)]
pub struct ThreadListItem {
    /// The thread root event.
    ///
    /// Contains the event ID, timestamp, sender, sender profile, and parsed
    /// content of the thread's root message. Use `root_event.event_id` to open
    /// a per-thread `Timeline` or to navigate the user to the thread view.
    root_event: ThreadListItemEvent,

    /// The latest event in the thread (i.e. the most recent reply), if
    /// available.
    ///
    /// Initially populated from the server's bundled thread summary and
    /// updated in real time as new events arrive via sync or back-pagination.
    latest_event: Option<ThreadListItemEvent>,

    /// The number of replies in this thread (excluding the root event).
    ///
    /// Initially populated from the server's bundled thread summary and
    /// updated in real time as new events arrive via sync.
    num_replies: u32,
}

impl From<UIThreadListItem> for ThreadListItem {
    fn from(item: UIThreadListItem) -> Self {
        Self {
            root_event: item.root_event.into(),
            latest_event: item.latest_event.map(Into::into),
            num_replies: item.num_replies,
        }
    }
}

/// Information about an event in a thread (either the root or the latest
/// reply).
#[derive(uniffi::Record)]
pub struct ThreadListItemEvent {
    /// The event ID.
    pub event_id: String,
    /// The timestamp of the event.
    pub timestamp: Timestamp,
    /// The sender of the event.
    pub sender: String,
    /// The sender's profile details.
    pub sender_profile: ProfileDetails,
    /// Whether the event was sent by the current user.
    pub is_own: bool,
    /// The content of the event, if available.
    pub content: Option<TimelineItemContent>,
}

impl From<UIThreadListItemEvent> for ThreadListItemEvent {
    fn from(event: UIThreadListItemEvent) -> Self {
        Self {
            event_id: event.event_id.to_string(),
            timestamp: event.timestamp.into(),
            sender: event.sender.to_string(),
            is_own: event.is_own,
            sender_profile: event.sender_profile.into(),
            content: event.content.map(Into::into),
        }
    }
}

/// Listener for changes to the [`ThreadListService`] pagination state.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ThreadListPaginationStateListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, state: UIThreadListPaginationState);
}

/// Listener for changes to the [`ThreadListService`] item list.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ThreadListEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, diff: Vec<ThreadListUpdate>);
}

/// A diff applied to the observable thread list.
///
/// Mirrors [`eyeball_im::VectorDiff`] for [`ThreadListItem`].
#[derive(uniffi::Enum)]
pub enum ThreadListUpdate {
    /// New items were appended at the back.
    Append { values: Vec<ThreadListItem> },
    /// The list was cleared.
    Clear,
    /// A new item was prepended at the front.
    PushFront { value: ThreadListItem },
    /// A new item was appended at the back.
    PushBack { value: ThreadListItem },
    /// The first item was removed.
    PopFront,
    /// The last item was removed.
    PopBack,
    /// An item was inserted at the given position.
    Insert { index: u32, value: ThreadListItem },
    /// The item at the given position was replaced.
    Set { index: u32, value: ThreadListItem },
    /// The item at the given position was removed.
    Remove { index: u32 },
    /// The list was truncated to the given length.
    Truncate { length: u32 },
    /// The whole list was replaced with new items.
    Reset { values: Vec<ThreadListItem> },
}

impl From<VectorDiff<UIThreadListItem>> for ThreadListUpdate {
    fn from(diff: VectorDiff<UIThreadListItem>) -> Self {
        match diff {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(Into::into).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => Self::PushFront { value: value.into() },
            VectorDiff::PushBack { value } => Self::PushBack { value: value.into() },
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: index as u32, value: value.into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: index as u32, value: value.into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: index as u32 },
            VectorDiff::Truncate { length } => Self::Truncate { length: length as u32 },
            VectorDiff::Reset { values } => {
                Self::Reset { values: values.into_iter().map(Into::into).collect() }
            }
        }
    }
}

/// A high-level, reactive, paginated list of threads for a room.
///
/// `ThreadListService` is the FFI-facing wrapper around
/// [`matrix_sdk_ui::timeline::thread_list_service::ThreadListService`]. It
/// maintains an observable list of [`ThreadListItem`]s and exposes a
/// pagination state publisher, making it straightforward to build reactive UIs
/// on top of the thread list.
///
/// Obtain an instance via [`Room::thread_list_service`].
#[derive(uniffi::Object)]
pub struct ThreadListService {
    inner: UIThreadListService,
}

impl ThreadListService {
    pub(crate) fn new(room: &matrix_sdk::Room) -> Self {
        Self { inner: room.thread_list_service() }
    }
}

#[matrix_sdk_ffi_macros::export]
impl ThreadListService {
    /// Returns a snapshot of the current pagination state.
    pub fn pagination_state(&self) -> UIThreadListPaginationState {
        self.inner.pagination_state()
    }

    /// Subscribes to changes in the pagination state.
    ///
    /// The `listener` is called once for every state transition. The returned
    /// [`TaskHandle`] keeps the subscription alive
    pub fn subscribe_to_pagination_state_updates(
        &self,
        listener: Box<dyn ThreadListPaginationStateListener>,
    ) -> Arc<TaskHandle> {
        let mut subscriber = self.inner.subscribe_to_pagination_state_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = subscriber.next().await {
                listener.on_update(state);
            }
        })))
    }

    /// Returns a snapshot of the current thread list items.
    pub fn items(&self) -> Vec<ThreadListItem> {
        self.inner.items().into_iter().map(Into::into).collect()
    }

    /// Subscribes to changes in the thread list.
    ///
    /// The `listener` receives an initial `Reset` diff containing all currently
    /// loaded items, followed by subsequent diffs as the list changes.
    pub fn subscribe_to_items_updates(
        &self,
        listener: Box<dyn ThreadListEntriesListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_items_updates();

        // Emit the current snapshot immediately so the caller starts with a
        // consistent view of the list.
        listener.on_update(vec![ThreadListUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }

    /// Fetches the next page of threads and appends the results to the list.
    ///
    /// This is a no-op when the list is already loading or the end has been
    /// reached.
    pub async fn paginate(&self) -> Result<(), ClientError> {
        self.inner.paginate().await.map_err(|e| match e {
            UIThreadListServiceError::Sdk(sdk_err) => ClientError::from(sdk_err),
        })
    }

    /// Resets the service back to its initial, empty state.
    ///
    /// Clears all loaded items, discards the pagination token, and sets the
    /// state to `Idle { end_reached: false }`. The next call to
    /// [`Self::paginate`] will restart from the beginning of the thread list.
    pub async fn reset(&self) {
        self.inner.reset().await;
    }
}
