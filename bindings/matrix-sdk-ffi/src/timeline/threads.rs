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

use matrix_sdk::room::{
    ListThreadsOptions as SdkListThreadsOptions, ThreadSubscription as SdkThreadSubscription,
};
use matrix_sdk_ui::timeline::threads::{
    ThreadList as UIThreadList, ThreadListItem as UIThreadListItem,
};
use ruma::api::client::threads::get_threads::v1::IncludeThreads as SdkIncludeThreads;

use crate::{
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

/// A structure wrapping a Thread List endpoint response i.e.
/// [`ThreadListItem`]s and the current pagination token.
#[derive(uniffi::Record)]
pub struct ThreadList {
    /// The events that are thread roots in the current batch.
    pub items: Vec<ThreadListItem>,

    /// Token to paginate backwards in a subsequent query to
    /// [`Room::list_threads`].
    pub prev_batch_token: Option<String>,
}

impl From<UIThreadList> for ThreadList {
    fn from(list: UIThreadList) -> Self {
        Self {
            items: list.items.into_iter().map(Into::into).collect(),
            prev_batch_token: list.prev_batch_token,
        }
    }
}

/// An individual Thread as retrieved from through Thread List API.
#[derive(uniffi::Record)]
pub struct ThreadListItem {
    root_event_id: String,
    timestamp: Timestamp,
    sender: String,
    sender_profile: ProfileDetails,
    is_own: bool,
    content: Option<TimelineItemContent>,
}

impl From<UIThreadListItem> for ThreadListItem {
    fn from(root: UIThreadListItem) -> Self {
        Self {
            root_event_id: root.root_event_id.to_string(),
            timestamp: root.timestamp.into(),
            sender: root.sender.to_string(),
            is_own: root.is_own,
            sender_profile: root.sender_profile.into(),
            content: root.content.map(Into::into),
        }
    }
}
