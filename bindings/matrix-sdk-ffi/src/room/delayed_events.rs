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

use ruma::api::client::delayed_events::update_delayed_event::UpdateAction;

use super::Room;
use crate::error::ClientError;

/// What to do with a delayed event that hasn't been distributed to the room
/// yet ([MSC4140]).
///
/// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
#[derive(Clone, Copy, Debug, uniffi::Enum)]
pub enum DelayedEventUpdateAction {
    /// Cancel the delayed event: it will never be sent to the room.
    Cancel,
    /// Restart the delay timeout, keeping the event scheduled.
    Restart,
    /// Send the event to the room right away.
    Send,
}

impl From<DelayedEventUpdateAction> for UpdateAction {
    fn from(value: DelayedEventUpdateAction) -> Self {
        match value {
            DelayedEventUpdateAction::Cancel => Self::Cancel,
            DelayedEventUpdateAction::Restart => Self::Restart,
            DelayedEventUpdateAction::Send => Self::Send,
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Send a delayed message-like event to the room ([MSC4140]).
    ///
    /// The homeserver holds on to the event and only distributes it to the room
    /// once `delay_ms` has elapsed, unless the delayed event is updated
    /// beforehand with [`Room::update_delayed_event`].
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
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    pub async fn send_delayed_event(
        &self,
        event_type: String,
        content: String,
        delay_ms: u64,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let response = self
            .inner
            .send_raw(&event_type, content_json)
            .into_delayed(Duration::from_millis(delay_ms))
            .await?;

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
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    pub async fn send_delayed_state_event(
        &self,
        event_type: String,
        state_key: String,
        content: String,
        delay_ms: u64,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let response = self
            .inner
            .send_state_event_raw(&event_type, &state_key, content_json)
            .into_delayed(Duration::from_millis(delay_ms))
            .await?;

        Ok(response.delay_id)
    }

    /// Cancel, restart or immediately send a previously scheduled delayed
    /// event ([MSC4140]).
    ///
    /// # Arguments
    ///
    /// * `delay_id` - The identifier returned by [`Room::send_delayed_event`]
    ///   or [`Room::send_delayed_state_event`].
    ///
    /// * `action` - What should happen to the delayed event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    pub async fn update_delayed_event(
        &self,
        delay_id: String,
        action: DelayedEventUpdateAction,
    ) -> Result<(), ClientError> {
        self.inner.update_delayed_event(delay_id, action.into()).await?;

        Ok(())
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
    /// Returns the `delay_id` identifying the scheduled event.
    ///
    /// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
    /// [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354
    /// [`Client::is_sticky_events_supported`]: crate::client::Client::is_sticky_events_supported
    pub async fn send_sticky_delayed_event(
        &self,
        event_type: String,
        content: String,
        delay_ms: u64,
        sticky_duration_ms: u64,
    ) -> Result<String, ClientError> {
        let content_json: serde_json::Value =
            serde_json::from_str(&content).map_err(matrix_sdk::Error::SerdeJson)?;

        let response = self
            .inner
            .send_raw(&event_type, content_json)
            .with_sticky_duration(Duration::from_millis(sticky_duration_ms))
            .into_delayed(Duration::from_millis(delay_ms))
            .await?;

        Ok(response.delay_id)
    }
}
