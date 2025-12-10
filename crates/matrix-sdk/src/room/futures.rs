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
// See the License for the specific language governing permissions and
// limitations under the License.

//! Named futures returned from methods on types in [the `room` module][super].

#![deny(unreachable_pub)]

#[cfg(feature = "experimental-encrypted-state-events")]
use std::borrow::Borrow;
use std::future::IntoFuture;

use eyeball::SharedObservable;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use matrix_sdk_common::boxed_into_future;
use mime::Mime;
#[cfg(doc)]
use ruma::events::{MessageLikeUnsigned, SyncMessageLikeEvent};
use ruma::{
    OwnedTransactionId, TransactionId,
    api::client::message::send_message_event,
    assign,
    events::{AnyMessageLikeEventContent, MessageLikeEventContent},
    serde::Raw,
};
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::{
    api::client::state::send_state_event,
    events::{AnyStateEventContent, StateEventContent},
};
use tracing::{Instrument, Span, info, trace};

use super::Room;
#[cfg(feature = "experimental-encrypted-state-events")]
use crate::utils::IntoRawStateEventContent;
use crate::{
    Result, TransmissionProgress, attachment::AttachmentConfig, config::RequestConfig,
    utils::IntoRawMessageLikeEventContent,
};

/// The result of the [`Room::send`] future
#[derive(Debug)]
pub struct SendMessageLikeEventResult {
    /// The response
    pub response: send_message_event::v3::Response,
    /// The encryption info, if the event was encrypted
    pub encryption_info: Option<EncryptionInfo>,
}

/// Future returned by [`Room::send`].
#[allow(missing_debug_implementations)]
pub struct SendMessageLikeEvent<'a> {
    room: &'a Room,
    event_type: String,
    content: serde_json::Result<serde_json::Value>,
    transaction_id: Option<OwnedTransactionId>,
    request_config: Option<RequestConfig>,
}

impl<'a> SendMessageLikeEvent<'a> {
    pub(crate) fn new(room: &'a Room, content: impl MessageLikeEventContent) -> Self {
        let event_type = content.event_type().to_string();
        let content = serde_json::to_value(&content);
        Self { room, event_type, content, transaction_id: None, request_config: None }
    }

    /// Set a transaction ID for this event.
    ///
    /// Since sending message-like events always requires a transaction ID, one
    /// is generated if this method is not called.
    ///
    /// The transaction ID is a locally-unique ID describing a message
    /// transaction with the homeserver.
    ///
    /// * On the sending side, this field is used for re-trying earlier failed
    ///   transactions. Subsequent messages *must never* re-use an earlier
    ///   transaction ID.
    /// * On the receiving side, the field is used for recognizing our own
    ///   messages when they arrive down the sync: the server includes the ID in
    ///   the [`MessageLikeUnsigned`] field `transaction_id` of the
    ///   corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///   device. Other devices will not see it. This is then used to ignore
    ///   events sent by our own device and/or to implement local echo.
    pub fn with_transaction_id(mut self, txn_id: OwnedTransactionId) -> Self {
        self.transaction_id = Some(txn_id);
        self
    }

    /// Assign a given [`RequestConfig`] to configure how this request should
    /// behave with respect to the network.
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = Some(request_config);
        self
    }
}

impl<'a> IntoFuture for SendMessageLikeEvent<'a> {
    type Output = Result<SendMessageLikeEventResult>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { room, event_type, content, transaction_id, request_config } = self;
        Box::pin(async move {
            let content = content?;
            assign!(room.send_raw(&event_type, content), { transaction_id, request_config }).await
        })
    }
}

/// Future returned by [`Room::send_raw`].
#[allow(missing_debug_implementations)]
pub struct SendRawMessageLikeEvent<'a> {
    room: &'a Room,
    event_type: &'a str,
    content: Raw<AnyMessageLikeEventContent>,
    tracing_span: Span,
    transaction_id: Option<OwnedTransactionId>,
    request_config: Option<RequestConfig>,
}

impl<'a> SendRawMessageLikeEvent<'a> {
    pub(crate) fn new(
        room: &'a Room,
        event_type: &'a str,
        content: impl IntoRawMessageLikeEventContent,
    ) -> Self {
        let content = content.into_raw_message_like_event_content();
        Self {
            room,
            event_type,
            content,
            tracing_span: Span::current(),
            transaction_id: None,
            request_config: None,
        }
    }

    /// Set a transaction ID for this event.
    ///
    /// Since sending message-like events always requires a transaction ID, one
    /// is generated if this method is not called.
    ///
    /// * On the sending side, this field is used for re-trying earlier failed
    ///   transactions. Subsequent messages *must never* re-use an earlier
    ///   transaction ID.
    /// * On the receiving side, the field is used for recognizing our own
    ///   messages when they arrive down the sync: the server includes the ID in
    ///   the [`MessageLikeUnsigned`] field `transaction_id` of the
    ///   corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///   device. Other devices will not see it. This is then used to ignore
    ///   events sent by our own device and/or to implement local echo.
    pub fn with_transaction_id(mut self, txn_id: &TransactionId) -> Self {
        self.transaction_id = Some(txn_id.to_owned());
        self
    }

    /// Assign a given [`RequestConfig`] to configure how this request should
    /// behave with respect to the network.
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = Some(request_config);
        self
    }
}

impl<'a> IntoFuture for SendRawMessageLikeEvent<'a> {
    type Output = Result<SendMessageLikeEventResult>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        #[cfg_attr(not(feature = "e2e-encryption"), allow(unused_mut))]
        let Self {
            room,
            mut event_type,
            mut content,
            tracing_span,
            transaction_id,
            request_config,
        } = self;

        let fut = async move {
            room.ensure_room_joined()?;

            let txn_id = transaction_id.unwrap_or_else(TransactionId::new);
            Span::current().record("transaction_id", tracing::field::debug(&txn_id));

            #[cfg(not(feature = "e2e-encryption"))]
            trace!("Sending plaintext event to room because we don't have encryption support.");

            #[cfg(feature = "e2e-encryption")]
            let mut encryption_info: Option<EncryptionInfo> = None;
            #[cfg(not(feature = "e2e-encryption"))]
            let encryption_info: Option<EncryptionInfo> = None;

            #[cfg(feature = "e2e-encryption")]
            if room.latest_encryption_state().await?.is_encrypted() {
                Span::current().record("is_room_encrypted", true);
                // Reactions are currently famously not encrypted, skip encrypting
                // them until they are.
                if event_type == "m.reaction" {
                    trace!("Sending plaintext event because of the event type.");
                } else {
                    trace!(
                        room_id = ?room.room_id(),
                        "Sending encrypted event because the room is encrypted.",
                    );

                    ensure_room_encryption_ready(room).await?;

                    let olm = room.client.olm_machine().await;
                    let olm = olm.as_ref().expect("Olm machine wasn't started");

                    let result =
                        olm.encrypt_room_event_raw(room.room_id(), event_type, &content).await?;
                    content = result.content.cast();
                    encryption_info = Some(result.encryption_info);
                    event_type = "m.room.encrypted";
                }
            } else {
                Span::current().record("is_room_encrypted", false);
                trace!("Sending plaintext event because the room is NOT encrypted.");
            }

            let request = send_message_event::v3::Request::new_raw(
                room.room_id().to_owned(),
                txn_id,
                event_type.into(),
                content,
            );

            let response = room.client.send(request).with_request_config(request_config).await?;

            Span::current().record("event_id", tracing::field::debug(&response.event_id));
            info!("Sent event in room");

            Ok(SendMessageLikeEventResult { response, encryption_info })
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

/// Future returned by [`Room::send_attachment`].
#[allow(missing_debug_implementations)]
pub struct SendAttachment<'a> {
    room: &'a Room,
    filename: String,
    content_type: &'a Mime,
    data: Vec<u8>,
    config: AttachmentConfig,
    tracing_span: Span,
    send_progress: SharedObservable<TransmissionProgress>,
    store_in_cache: bool,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        room: &'a Room,
        filename: String,
        content_type: &'a Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> Self {
        Self {
            room,
            filename,
            content_type,
            data,
            config,
            tracing_span: Span::current(),
            send_progress: Default::default(),
            store_in_cache: false,
        }
    }

    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }

    /// Whether the sent attachment should be stored in the cache or not.
    ///
    /// If set to true, then retrieving the data for the attachment will result
    /// in a cache hit immediately after upload.
    pub fn store_in_cache(mut self) -> Self {
        self.store_in_cache = true;
        self
    }
}

impl<'a> IntoFuture for SendAttachment<'a> {
    type Output = Result<send_message_event::v3::Response>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            room,
            filename,
            content_type,
            data,
            config,
            tracing_span,
            send_progress,
            store_in_cache,
        } = self;
        let fut = async move {
            room.prepare_and_send_attachment(
                filename,
                content_type,
                data,
                config,
                send_progress,
                store_in_cache,
            )
            .await
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

/// Future returned by [`Room::send_state_event_raw`].
#[cfg(feature = "experimental-encrypted-state-events")]
#[allow(missing_debug_implementations)]
pub struct SendRawStateEvent<'a> {
    room: &'a Room,
    event_type: &'a str,
    state_key: &'a str,
    content: Raw<AnyStateEventContent>,
    tracing_span: Span,
    request_config: Option<RequestConfig>,
}

#[cfg(feature = "experimental-encrypted-state-events")]
impl<'a> SendRawStateEvent<'a> {
    pub(crate) fn new(
        room: &'a Room,
        event_type: &'a str,
        state_key: &'a str,
        content: impl IntoRawStateEventContent,
    ) -> Self {
        let content = content.into_raw_state_event_content();
        Self {
            room,
            event_type,
            state_key,
            content,
            tracing_span: Span::current(),
            request_config: None,
        }
    }

    /// Assign a given [`RequestConfig`] to configure how this request should
    /// behave with respect to the network.
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = Some(request_config);
        self
    }

    /// Determines whether the inner state event should be encrypted before
    /// sending.
    ///
    /// This method checks two conditions:
    /// 1. Whether the room supports encrypted state events, by inspecting the
    ///    room's encryption state.
    /// 2. Whether the event type is considered "critical" or excluded from
    ///    encryption under MSC4362.
    ///
    /// # Returns
    ///
    /// Returns `true` if the event should be encrypted, otherwise returns
    /// `false`.
    fn should_encrypt(room: &Room, event_type: &str) -> bool {
        if !room.encryption_state().is_state_encrypted() {
            trace!("Sending plaintext event as the room does NOT support encrypted state events.");
            return false;
        }

        // Check the event is not critical.
        if matches!(
            event_type,
            "m.room.create"
                | "m.room.member"
                | "m.room.join_rules"
                | "m.room.power_levels"
                | "m.room.third_party_invite"
                | "m.room.history_visibility"
                | "m.room.guest_access"
                | "m.room.encryption"
                | "m.space.child"
                | "m.space.parent"
        ) {
            trace!("Sending plaintext event as its type is excluded from encryption.");
            return false;
        }

        true
    }
}

#[cfg(feature = "experimental-encrypted-state-events")]
impl<'a> IntoFuture for SendRawStateEvent<'a> {
    type Output = Result<send_state_event::v3::Response>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { room, mut event_type, state_key, mut content, tracing_span, request_config } =
            self;

        let fut = async move {
            room.ensure_room_joined()?;

            let mut state_key = state_key.to_owned();

            if Self::should_encrypt(room, event_type) {
                use tracing::debug;

                Span::current().record("should_encrypt", true);
                debug!(
                    room_id = ?room.room_id(),
                    "Sending encrypted event because the room is encrypted.",
                );

                ensure_room_encryption_ready(room).await?;

                let olm = room.client.olm_machine().await;
                let olm = olm.as_ref().expect("Olm machine wasn't started");

                content = olm
                    .encrypt_state_event_raw(room.room_id(), event_type, &state_key, &content)
                    .await?
                    .cast_unchecked();

                state_key = format!("{event_type}:{state_key}");
                event_type = "m.room.encrypted";
            } else {
                Span::current().record("should_encrypt", false);
            }

            let request = send_state_event::v3::Request::new_raw(
                room.room_id().to_owned(),
                event_type.into(),
                state_key.to_owned(),
                content,
            );

            let response = room.client.send(request).with_request_config(request_config).await?;

            Span::current().record("event_id", tracing::field::debug(&response.event_id));
            info!("Sent event in room");

            Ok(response)
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

/// Future returned by `Room::send_state_event`.
#[allow(missing_debug_implementations)]
#[cfg(feature = "experimental-encrypted-state-events")]
pub struct SendStateEvent<'a> {
    room: &'a Room,
    event_type: String,
    state_key: String,
    content: serde_json::Result<serde_json::Value>,
    request_config: Option<RequestConfig>,
}

#[cfg(feature = "experimental-encrypted-state-events")]
impl<'a> SendStateEvent<'a> {
    pub(crate) fn new<C, K>(room: &'a Room, state_key: &K, content: C) -> Self
    where
        C: StateEventContent,
        C::StateKey: Borrow<K>,
        K: AsRef<str> + ?Sized,
    {
        let event_type = content.event_type().to_string();
        let state_key = state_key.as_ref().to_owned();
        let content = serde_json::to_value(&content);
        Self { room, event_type, state_key, content, request_config: None }
    }

    /// Assign a given [`RequestConfig`] to configure how this request should
    /// behave with respect to the network.
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = Some(request_config);
        self
    }
}

#[cfg(feature = "experimental-encrypted-state-events")]
impl<'a> IntoFuture for SendStateEvent<'a> {
    type Output = Result<send_state_event::v3::Response>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { room, state_key, event_type, content, request_config } = self;
        Box::pin(async move {
            let content = content?;
            assign!(room.send_state_event_raw(&event_type, &state_key, content), { request_config })
                .await
        })
    }
}

/// Ensures the room is ready for encrypted events to be sent.
#[cfg(feature = "e2e-encryption")]
async fn ensure_room_encryption_ready(room: &Room) -> Result<()> {
    if !room.are_members_synced() {
        room.sync_members().await?;
    }

    // Query keys in case we don't have them for newly synced members.
    //
    // Note we do it all the time, because we might have sync'd members before
    // sending a message (so didn't enter the above branch), but
    // could have not query their keys ever.
    room.query_keys_for_untracked_or_dirty_users().await?;

    room.preshare_room_key().await?;

    Ok(())
}
