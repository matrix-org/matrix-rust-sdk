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

//! Timeline item content for live location sharing (MSC3489).
//!
//! Live location sharing uses two event types:
//! - `org.matrix.msc3672.beacon_info` (state event): starts/stops a sharing
//!   session and creates the timeline item represented by
//!   [`LiveLocationState`].
//! - `org.matrix.msc3672.beacon` (message-like event): periodic location
//!   updates that are aggregated onto the parent [`LiveLocationState`] item.

use ruma::{
    MilliSecondsSinceUnixEpoch,
    events::{beacon_info::BeaconInfoEventContent, location::AssetType},
};

/// A single location update received from a beacon event.
///
/// Created from an `org.matrix.msc3672.beacon` message-like event and
/// aggregated onto the parent [`LiveLocationState`] timeline item.
#[derive(Clone, Debug)]
pub struct BeaconInfo {
    /// The geo URI carrying the user's coordinates (e.g.
    /// `"geo:51.5008,0.1247;u=35"`).
    pub(in crate::timeline) geo_uri: String,

    /// Timestamp of this location update (from the beacon event's
    /// `org.matrix.msc3488.ts` field).
    pub(in crate::timeline) ts: MilliSecondsSinceUnixEpoch,

    /// An optional human-readable description of the location.
    pub(in crate::timeline) description: Option<String>,
}

impl BeaconInfo {
    /// The geo URI of this location update.
    pub fn geo_uri(&self) -> &str {
        &self.geo_uri
    }

    /// The timestamp of this location update.
    pub fn ts(&self) -> MilliSecondsSinceUnixEpoch {
        self.ts
    }

    /// An optional human-readable description of this location.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// The state of a live location sharing session.
///
/// Created when a `org.matrix.msc3672.beacon_info` state event is received.
/// Subsequent `org.matrix.msc3672.beacon` message-like events are aggregated
/// onto this item, appending to [`LiveLocationState::locations`].
///
/// When a user stops sharing (a new `beacon_info` with `live: false` arrives)
/// a *separate* timeline item is created for the stop event. The original
/// item's liveness can be checked via [`LiveLocationState::is_live`], which
/// internally checks both the `live` flag and the session timeout.
#[derive(Clone, Debug)]
pub struct LiveLocationState {
    /// The content of the `beacon_info` state event that created this item.
    pub(in crate::timeline) beacon_info: BeaconInfoEventContent,

    /// All location updates aggregated onto this session, kept sorted by
    /// timestamp.
    pub(in crate::timeline) locations: Vec<BeaconInfo>,
}

impl LiveLocationState {
    /// Create a new [`LiveLocationState`] from the given
    /// [`BeaconInfoEventContent`].
    pub fn new(beacon_info: BeaconInfoEventContent) -> Self {
        Self { beacon_info, locations: Vec::new() }
    }

    /// Add a location update. Keeps the internal list sorted by timestamp so
    /// that [`LiveLocationState::latest_location`] always returns the most
    /// recent one.
    pub(in crate::timeline) fn add_location(&mut self, location: BeaconInfo) {
        match self.locations.binary_search_by_key(&location.ts, |l| l.ts) {
            Ok(_) => (), // Duplicate timestamp, do nothing.
            Err(index) => self.locations.insert(index, location),
        }
    }

    /// Remove the location update with the given timestamp. Used when
    /// unapplying an aggregation (e.g. event cache moves an event).
    pub(in crate::timeline) fn remove_location(&mut self, ts: MilliSecondsSinceUnixEpoch) {
        self.locations.retain(|l| l.ts != ts);
    }

    /// All accumulated location updates, sorted by timestamp (oldest first).
    pub fn locations(&self) -> &[BeaconInfo] {
        &self.locations
    }

    /// The most recent location update, if any have been received.
    pub fn latest_location(&self) -> Option<&BeaconInfo> {
        self.locations.last()
    }

    /// Whether this live location share is still active.
    ///
    /// Returns `false` once the `live` flag has been set to `false` **or**
    /// the session's timeout has elapsed.
    pub fn is_live(&self) -> bool {
        self.beacon_info.is_live()
    }

    /// The timestamp when this live location sharing session started
    /// (from the `org.matrix.msc3488.ts` field of the originating
    /// `beacon_info` state event).
    ///
    /// This marks the *beginning* of the session. The session expires at
    /// `ts + timeout` — see [`LiveLocationState::is_live`] and
    /// [`LiveLocationState::timeout`].
    pub fn ts(&self) -> MilliSecondsSinceUnixEpoch {
        self.beacon_info.ts
    }

    /// An optional human-readable description for this sharing session
    /// (from the originating `beacon_info` event).
    pub fn description(&self) -> Option<&str> {
        self.beacon_info.description.as_deref()
    }

    /// The duration that the location sharing will be live.
    ///
    /// Meaning that the location will stop being shared at `ts + timeout`.
    pub fn timeout(&self) -> std::time::Duration {
        self.beacon_info.timeout
    }

    /// The asset type of the beacon (e.g. `Sender` for the user's own
    /// location, `Pin` for a fixed point of interest).
    pub fn asset_type(&self) -> AssetType {
        self.beacon_info.asset.type_.clone()
    }

    /// Update this session with a stop `beacon_info` event (one where
    /// `live` is `false`). This replaces the stored content so that
    /// [`LiveLocationState::is_live`] will return `false`.
    pub(in crate::timeline) fn stop(&mut self, beacon_info: BeaconInfoEventContent) {
        assert!(!beacon_info.is_live(), "A stop `beacon_info` event must not be live.");
        self.beacon_info = beacon_info;
    }
}
