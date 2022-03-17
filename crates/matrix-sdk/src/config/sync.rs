// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use ruma::api::client::sync::sync_events;

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
/// Settings for a sync call.
pub struct SyncSettings<'a> {
    pub(crate) filter: Option<sync_events::v3::Filter<'a>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: bool,
}

impl<'a> Default for SyncSettings<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> SyncSettings<'a> {
    /// Create new default sync settings.
    #[must_use]
    pub fn new() -> Self {
        Self { filter: None, timeout: Some(DEFAULT_SYNC_TIMEOUT), token: None, full_state: false }
    }

    /// Set the sync token.
    ///
    /// # Arguments
    ///
    /// * `token` - The sync token that should be used for the sync call.
    #[must_use]
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the maximum time the server can wait, in milliseconds, before
    /// responding to the sync request.
    ///
    /// # Arguments
    ///
    /// * `timeout` - The time the server is allowed to wait.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the sync filter.
    /// It can be either the filter ID, or the definition for the filter.
    ///
    /// # Arguments
    ///
    /// * `filter` - The filter configuration that should be used for the sync
    ///   call.
    #[must_use]
    pub fn filter(mut self, filter: sync_events::v3::Filter<'a>) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Should the server return the full state from the start of the timeline.
    ///
    /// This does nothing if no sync token is set.
    ///
    /// # Arguments
    /// * `full_state` - A boolean deciding if the server should return the full
    ///   state or not.
    #[must_use]
    pub fn full_state(mut self, full_state: bool) -> Self {
        self.full_state = full_state;
        self
    }
}
