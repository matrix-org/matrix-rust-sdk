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

//! Delayed events ([MSC4140]).
//!
//! [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140

use std::time::Duration;

use super::{RequestConfigOverride, Room};
use crate::error::ClientError;

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Send a delayed message-like event to the room ([MSC4140]).
    ///
    /// The homeserver holds on to the event and only distributes it to the room
    /// once `delay_ms` has elapsed, unless the delayed event is updated
    /// beforehand with [`Client::update_delayed_event`].
    ///
    /// If the room is encrypted, the content is encrypted when the event is
    /// scheduled, like [`Room::send_raw`] does. A member joining the room
    /// before the delay elapses won't be able to decrypt it.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event to send.
    ///
    /// * `content` - The content of the event to send encoded as a JSON string.
    ///
    /// * `delay_ms` - How long, in milliseconds, the homeserver should hold on
    ///   to the event.
    ///
    /// * `request_config_override` - Overrides the client's request config for
    ///   this request. Fields that are not set keep the client's values.
    ///
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    /// [`Client::update_delayed_event`]: crate::client::Client::update_delayed_event
    #[uniffi::method(default(request_config_override = None))]
    pub async fn send_delayed_event(
        &self,
        event_type: String,
        content: String,
        delay_ms: u64,
        request_config_override: Option<RequestConfigOverride>,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let mut request = self.inner.send_raw(&event_type, content_json);
        if let Some(request_config_override) = request_config_override {
            request = request.with_request_config(
                request_config_override.apply_to(self.inner.client().request_config()),
            );
        }
        let response = request.into_delayed(Duration::from_millis(delay_ms)).await?;

        Ok(response.delay_id)
    }

    /// Send a delayed state event to the room ([MSC4140]).
    ///
    /// See [`Room::send_delayed_event`] for the semantics of delayed events.
    /// Unlike message-like events, state events can't be sticky.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the state event to send.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    ///   this piece of room state. This is often an empty string.
    ///
    /// * `content` - The content of the event to send encoded as a JSON string.
    ///
    /// * `delay_ms` - How long, in milliseconds, the homeserver should hold on
    ///   to the event.
    ///
    /// * `request_config_override` - Overrides the client's request config for
    ///   this request. Fields that are not set keep the client's values.
    ///
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    #[uniffi::method(default(request_config_override = None))]
    pub async fn send_delayed_state_event(
        &self,
        event_type: String,
        state_key: String,
        content: String,
        delay_ms: u64,
        request_config_override: Option<RequestConfigOverride>,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let mut request = self.inner.send_state_event_raw(&event_type, &state_key, content_json);
        if let Some(request_config_override) = request_config_override {
            request = request.with_request_config(
                request_config_override.apply_to(self.inner.client().request_config()),
            );
        }
        let response = request.into_delayed(Duration::from_millis(delay_ms)).await?;

        Ok(response.delay_id)
    }
}

#[cfg(feature = "unstable-msc4354")]
#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Send a delayed message-like event to the room ([MSC4140]) that becomes
    /// sticky ([MSC4354]) once the homeserver sends it.
    ///
    /// See [`Room::send_delayed_event`] for the semantics of delayed events.
    ///
    /// Note that if the homeserver doesn't support sticky events, it will
    /// ignore the duration and send the event unsticky. Server support can be
    /// checked with [`Client::is_sticky_events_supported`].
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event to send.
    ///
    /// * `content` - The content of the event to send encoded as a JSON string.
    ///
    /// * `delay_ms` - How long, in milliseconds, the homeserver should hold on
    ///   to the event.
    ///
    /// * `sticky_duration_ms` - How long the event stays sticky for once it is
    ///   sent, in milliseconds, clamped to one hour.
    ///
    /// * `request_config_override` - Overrides the client's request config for
    ///   this request. Fields that are not set keep the client's values.
    ///
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    /// [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354
    /// [`Client::is_sticky_events_supported`]: crate::client::Client::is_sticky_events_supported
    #[uniffi::method(default(request_config_override = None))]
    pub async fn send_sticky_delayed_event(
        &self,
        event_type: String,
        content: String,
        delay_ms: u64,
        sticky_duration_ms: u64,
        request_config_override: Option<RequestConfigOverride>,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let mut request = self
            .inner
            .send_raw(&event_type, content_json)
            .with_sticky_duration(Duration::from_millis(sticky_duration_ms));
        if let Some(request_config_override) = request_config_override {
            request = request.with_request_config(
                request_config_override.apply_to(self.inner.client().request_config()),
            );
        }
        let response = request.into_delayed(Duration::from_millis(delay_ms)).await?;

        Ok(response.delay_id)
    }
}
