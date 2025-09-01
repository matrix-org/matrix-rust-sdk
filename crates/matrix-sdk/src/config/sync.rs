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

use std::{fmt, time::Duration};

use matrix_sdk_common::debug::DebugStructExt;
use ruma::{api::client::sync::sync_events, presence::PresenceState};

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Token to be used in the next sync request.
#[derive(Clone, Default, Debug)]
pub enum SyncToken {
    /// Provide a specific token.
    Specific(String),
    /// Enforce no tokens at all.
    NoToken,
    /// Use a previous token if the client saw one in the past, and none
    /// otherwise.
    ///
    /// This is the default value.
    #[default]
    ReusePrevious,
}

impl<T> From<T> for SyncToken
where
    T: Into<String>,
{
    fn from(token: T) -> SyncToken {
        SyncToken::Specific(token.into())
    }
}

impl SyncToken {
    /// Convert a token that may exist into a [`SyncToken`]
    pub fn from_optional_token(maybe_token: Option<String>) -> SyncToken {
        match maybe_token {
            Some(token) => SyncToken::Specific(token),
            None => SyncToken::default(),
        }
    }
}

/// Settings for a sync call.
#[derive(Clone)]
pub struct SyncSettings {
    // Filter is pretty big at 1000 bytes, box it to reduce stack size
    pub(crate) filter: Option<Box<sync_events::v3::Filter>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) ignore_timeout_on_first_sync: bool,
    pub(crate) token: SyncToken,
    pub(crate) full_state: bool,
    pub(crate) set_presence: PresenceState,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            filter,
            timeout,
            ignore_timeout_on_first_sync,
            token: _,
            full_state,
            set_presence,
        } = self;
        f.debug_struct("SyncSettings")
            .maybe_field("filter", filter)
            .maybe_field("timeout", timeout)
            .field("ignore_timeout_on_first_sync", ignore_timeout_on_first_sync)
            .field("full_state", full_state)
            .field("set_presence", set_presence)
            .finish()
    }
}

impl SyncSettings {
    /// Create new default sync settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            filter: None,
            timeout: Some(DEFAULT_SYNC_TIMEOUT),
            ignore_timeout_on_first_sync: false,
            token: SyncToken::default(),
            full_state: false,
            set_presence: PresenceState::Online,
        }
    }

    /// Set the sync token.
    ///
    /// # Arguments
    ///
    /// * `token` - The sync token that should be used for the sync call.
    #[must_use]
    pub fn token(mut self, token: impl Into<SyncToken>) -> Self {
        self.token = token.into();
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

    /// Whether to ignore the `timeout` the first time that the `/sync` endpoint
    /// is called.
    ///
    /// If there is no new data to show, the server will wait until the end of
    /// `timeout` before returning a response. It can be an undesirable
    /// behavior when starting a client and informing the user that we are
    /// "catching up" while waiting for the first response.
    ///
    /// By not setting a `timeout` on the first request to `/sync`, the
    /// homeserver should reply immediately, whether the response is empty or
    /// not.
    ///
    /// Note that this setting is ignored when calling [`Client::sync_once()`],
    /// because there is no loop happening.
    ///
    /// # Arguments
    ///
    /// * `ignore` - Whether to ignore the `timeout` the first time that the
    ///   `/sync` endpoint is called.
    ///
    /// [`Client::sync_once()`]: crate::Client::sync_once
    #[must_use]
    pub fn ignore_timeout_on_first_sync(mut self, ignore: bool) -> Self {
        self.ignore_timeout_on_first_sync = ignore;
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
    pub fn filter(mut self, filter: sync_events::v3::Filter) -> Self {
        self.filter = Some(Box::new(filter));
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

    /// Set the presence state
    ///
    /// `PresenceState::Online` - The client is marked as being online. This is
    /// the default preset.
    ///
    /// `PresenceState::Offline` - The client is not marked as being online.
    ///
    /// `PresenceState::Unavailable` - The client is marked as being idle.
    ///
    /// # Arguments
    /// * `set_presence` - The `PresenceState` that the server should set for
    ///   the client.
    #[must_use]
    pub fn set_presence(mut self, presence: PresenceState) -> Self {
        self.set_presence = presence;
        self
    }
}
