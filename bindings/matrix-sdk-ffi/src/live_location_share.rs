// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::live_location_share::{
    LiveLocationShare as SdkLiveLocationShare, LiveLocationShares as SdkLiveLocationShares,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};

use crate::{ruma::LocationContent, runtime::get_runtime_handle, task_handle::TaskHandle};

/// Details of the last known location beacon.
#[derive(uniffi::Record)]
pub struct LastLocation {
    /// The most recent location content shared for this asset.
    pub location: LocationContent,
    /// The timestamp of when the location was updated.
    pub ts: u64,
}

/// Details of a user's live location share.
#[derive(uniffi::Record)]
pub struct LiveLocationShare {
    /// The asset's last known location.
    pub last_location: Option<LastLocation>,
    /// The user ID of the person sharing their live location.
    pub user_id: String,
    /// The time when location sharing started.
    pub start_ts: u64,
    /// The duration that the location sharing will be live.
    /// Meaning that the location will stop being shared at ts + timeout.
    pub timeout: u64,
}

/// An update to the list of active live location shares.
///
/// Corresponds to a [`VectorDiff`] on the underlying [`ObservableVector`].
///
/// [`ObservableVector`]: eyeball_im::ObservableVector
#[derive(uniffi::Enum)]
pub enum LiveLocationShareUpdate {
    Append { values: Vec<LiveLocationShare> },
    Clear,
    PushFront { value: LiveLocationShare },
    PushBack { value: LiveLocationShare },
    PopFront,
    PopBack,
    Insert { index: u32, value: LiveLocationShare },
    Set { index: u32, value: LiveLocationShare },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<LiveLocationShare> },
}

/// Listener for live location share updates.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait LiveLocationShareListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    /// Called with a batch of [`LiveLocationShareUpdate`]s whenever the list
    /// of active shares changes.
    fn on_update(&self, updates: Vec<LiveLocationShareUpdate>);
}

/// Tracks active live location shares in a room.
///
/// Holds the SDK [`SdkLiveLocationShares`] which keeps the beacon and
/// beacon_info event handlers registered for as long as this object is alive.
/// Call [`LiveLocationShares::subscribe`] to start receiving updates.
#[derive(uniffi::Object)]
pub struct LiveLocationShares {
    inner: SdkLiveLocationShares,
}

impl LiveLocationShares {
    pub fn new(inner: SdkLiveLocationShares) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl LiveLocationShares {
    /// Subscribe to changes in the list of active live location shares.
    ///
    /// Immediately calls `listener` with a `Reset` update containing the
    /// current snapshot (if non-empty), then calls it again for every
    /// subsequent change that arrives from sync.
    ///
    /// Returns a [`TaskHandle`] that, when dropped, stops the listener.
    /// The event handlers remain registered for as long as this
    /// [`LiveLocationShares`] object is alive.
    pub fn subscribe(&self, listener: Box<dyn LiveLocationShareListener>) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe();

        if !initial_values.is_empty() {
            listener.on_update(vec![LiveLocationShareUpdate::Reset {
                values: initial_values.into_iter().map(Into::into).collect(),
            }]);
        }

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }
}

impl From<SdkLiveLocationShare> for LiveLocationShare {
    fn from(share: SdkLiveLocationShare) -> Self {
        let start_ts = share.beacon_info.ts.0.into();
        let timeout = share.beacon_info.timeout.as_millis() as u64;
        let asset = share.beacon_info.asset.type_.into();
        let last_location = share.last_location.map(|l| LastLocation {
            location: LocationContent {
                body: "".to_owned(),
                geo_uri: l.location.uri.to_string(),
                description: None,
                zoom_level: None,
                asset,
            },
            ts: l.ts.0.into(),
        });
        LiveLocationShare { user_id: share.user_id.to_string(), last_location, start_ts, timeout }
    }
}

impl From<VectorDiff<SdkLiveLocationShare>> for LiveLocationShareUpdate {
    fn from(diff: VectorDiff<SdkLiveLocationShare>) -> Self {
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
