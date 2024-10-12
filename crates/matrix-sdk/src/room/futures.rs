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

use std::future::IntoFuture;

use eyeball::SharedObservable;
use matrix_sdk_common::boxed_into_future;
use mime::Mime;
#[cfg(doc)]
use ruma::events::{MessageLikeUnsigned, SyncMessageLikeEvent};
use ruma::{
    api::client::message::send_message_event,
    assign,
    events::{AnyMessageLikeEventContent, MessageLikeEventContent},
    serde::Raw,
    OwnedTransactionId, TransactionId,
};
use tracing::{info, trace, Instrument, Span};

use super::Room;
use crate::{
    attachment::AttachmentConfig, config::RequestConfig, utils::IntoRawMessageLikeEventContent,
    Result, TransmissionProgress,
};

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
    type Output = Result<send_message_event::v3::Response>;
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
    type Output = Result<send_message_event::v3::Response>;
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
            if room.is_encrypted().await? {
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

                    if !room.are_members_synced() {
                        room.sync_members().await?;
                    }

                    // Query keys in case we don't have them for newly synced members.
                    //
                    // Note we do it all the time, because we might have sync'd members before
                    // sending a message (so didn't enter the above branch), but
                    // could have not query their keys ever.
                    room.query_keys_for_untracked_users().await?;

                    room.preshare_room_key().await?;

                    let olm = room.client.olm_machine().await;
                    let olm = olm.as_ref().expect("Olm machine wasn't started");

                    content = olm
                        .encrypt_room_event_raw(room.room_id(), event_type, &content)
                        .await?
                        .cast();
                    event_type = "m.room.encrypted";
                }
            } else {
                Span::current().record("is_room_encrypted", false);
                trace!("Sending plaintext event because the room is NOT encrypted.",);
            };

            let request = send_message_event::v3::Request::new_raw(
                room.room_id().to_owned(),
                txn_id,
                event_type.into(),
                content,
            );

            let response = room.client.send(request, request_config).await?;

            Span::current().record("event_id", tracing::field::debug(&response.event_id));
            info!("Sent event in room");

            Ok(response)
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

/// Future returned by [`Room::send_attachment`].
#[allow(missing_debug_implementations)]
pub struct SendAttachment<'a> {
    room: &'a Room,
    filename: &'a str,
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
        filename: &'a str,
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
