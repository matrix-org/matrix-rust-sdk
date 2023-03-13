#[cfg(feature = "image-proc")]
use std::io::Cursor;
#[cfg(feature = "e2e-encryption")]
use std::sync::Arc;
use std::{borrow::Borrow, ops::Deref};

use matrix_sdk_common::instant::{Duration, Instant};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_common::locks::Mutex;
use mime::{self, Mime};
use ruma::{
    api::client::{
        membership::{
            ban_user,
            invite_user::{self, v3::InvitationRecipient},
            kick_user, Invite3pid,
        },
        message::send_message_event,
        read_marker::set_read_marker,
        receipt::create_receipt::{self, v3::ReceiptType},
        redact::redact_event,
        state::send_state_event,
        typing::create_typing_event::v3::{Request as TypingRequest, Typing},
    },
    assign,
    events::{
        receipt::ReceiptThread,
        room::{message::RoomMessageEventContent, power_levels::RoomPowerLevelsEventContent},
        EmptyStateKey, MessageLikeEventContent, StateEventContent,
    },
    serde::Raw,
    EventId, Int, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
};
use serde_json::Value;
use tracing::{debug, instrument};

use super::Left;
use crate::{
    attachment::AttachmentConfig,
    error::{Error, HttpResult},
    room::Common,
    BaseRoom, Client, Result, RoomState,
};
#[cfg(feature = "image-proc")]
use crate::{
    attachment::{generate_image_thumbnail, Thumbnail},
    error::ImageError,
};

const TYPING_NOTICE_TIMEOUT: Duration = Duration::from_secs(4);
const TYPING_NOTICE_RESEND_TIMEOUT: Duration = Duration::from_secs(3);

/// A room in the joined state.
///
/// The `JoinedRoom` contains all methods specific to a `Room` with
/// `RoomState::Joined`. Operations may fail once the underlying `Room` changes
/// `RoomState`.
#[derive(Debug, Clone)]
pub struct Joined {
    pub(crate) inner: Common,
}

impl Deref for Joined {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Joined {
    /// Create a new `room::Joined` if the underlying `BaseRoom` has
    /// `RoomState::Joined`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub(crate) fn new(client: &Client, room: BaseRoom) -> Option<Self> {
        if room.state() == RoomState::Joined {
            Some(Self { inner: Common::new(client.clone(), room) })
        } else {
            None
        }
    }

    /// Leave this room.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn leave(&self) -> Result<Left> {
        self.inner.leave().await
    }

    /// Ban the user with `UserId` from this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user to ban with `UserId`.
    ///
    /// * `reason` - The reason for banning this user.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn ban_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            ban_user::v3::Request::new(self.inner.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Kick a user out of this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user that should be kicked out of the
    ///   room.
    ///
    /// * `reason` - Optional reason why the room member is being kicked out.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn kick_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            kick_user::v3::Request::new(self.inner.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Invite the specified user by `UserId` to this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user to invite to the room.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn invite_user_by_id(&self, user_id: &UserId) -> Result<()> {
        let recipient = InvitationRecipient::UserId { user_id: user_id.to_owned() };

        let request = invite_user::v3::Request::new(self.inner.room_id().to_owned(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Invite the specified user by third party id to this room.
    ///
    /// # Arguments
    ///
    /// * `invite_id` - A third party id of a user to invite to the room.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn invite_user_by_3pid(&self, invite_id: Invite3pid) -> Result<()> {
        let recipient = InvitationRecipient::ThirdPartyId(invite_id);
        let request = invite_user::v3::Request::new(self.inner.room_id().to_owned(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Activate typing notice for this room.
    ///
    /// The typing notice remains active for 4s. It can be deactivate at any
    /// point by setting typing to `false`. If this method is called while
    /// the typing notice is active nothing will happen. This method can be
    /// called on every key stroke, since it will do nothing while typing is
    /// active.
    ///
    /// # Arguments
    ///
    /// * `typing` - Whether the user is typing or has stopped typing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    ///
    /// use matrix_sdk::ruma::api::client::typing::create_typing_event::v3::Typing;
    ///
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.typing_notice(true).await?
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn typing_notice(&self, typing: bool) -> Result<()> {
        // Only send a request to the homeserver if the old timeout has elapsed
        // or the typing notice changed state within the
        // TYPING_NOTICE_TIMEOUT
        let send = if let Some(typing_time) =
            self.client.inner.typing_notice_times.get(self.inner.room_id())
        {
            if typing_time.elapsed() > TYPING_NOTICE_RESEND_TIMEOUT {
                // We always reactivate the typing notice if typing is true or
                // we may need to deactivate it if it's
                // currently active if typing is false
                typing || typing_time.elapsed() <= TYPING_NOTICE_TIMEOUT
            } else {
                // Only send a request when we need to deactivate typing
                !typing
            }
        } else {
            // Typing notice is currently deactivated, therefore, send a request
            // only when it's about to be activated
            typing
        };

        if send {
            self.send_typing_notice(typing).await?;
        }

        Ok(())
    }

    #[instrument(name = "typing_notice", skip(self), parent = &self.client.inner.root_span)]
    async fn send_typing_notice(&self, typing: bool) -> Result<()> {
        let typing = if typing {
            self.client
                .inner
                .typing_notice_times
                .insert(self.inner.room_id().to_owned(), Instant::now());
            Typing::Yes(TYPING_NOTICE_TIMEOUT)
        } else {
            self.client.inner.typing_notice_times.remove(self.inner.room_id());
            Typing::No
        };

        let request = TypingRequest::new(
            self.inner.own_user_id().to_owned(),
            self.inner.room_id().to_owned(),
            typing,
        );

        self.client.send(request, None).await?;

        Ok(())
    }

    /// Send a request to set a single receipt.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of the receipt to set. Note that it is
    ///   possible to set the fully-read marker although it is technically not a
    ///   receipt.
    ///
    /// * `thread` - The thread where this receipt should apply, if any. Note
    ///   that this must be [`ReceiptThread::Unthreaded`] when sending a
    ///   [`ReceiptType::FullyRead`].
    ///
    /// * `event_id` - The `EventId` of the event to set the receipt on.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn send_single_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: OwnedEventId,
    ) -> Result<()> {
        let mut request = create_receipt::v3::Request::new(
            self.inner.room_id().to_owned(),
            receipt_type,
            event_id,
        );
        request.thread = thread;

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Send a request to set multiple receipts at once.
    ///
    /// # Arguments
    ///
    /// * `receipts` - The `Receipts` to send.
    ///
    /// If `receipts` is empty, this is a no-op.
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn send_multiple_receipts(&self, receipts: Receipts) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }

        let Receipts { fully_read, read_receipt, private_read_receipt } = receipts;
        let request = assign!(set_read_marker::v3::Request::new(self.inner.room_id().to_owned()), {
            fully_read,
            read_receipt,
            private_read_receipt,
        });

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Enable End-to-end encryption in this room.
    ///
    /// This method will be a noop if encryption is already enabled, otherwise
    /// sends a `m.room.encryption` state event to the room. This might fail if
    /// you don't have the appropriate power level to enable end-to-end
    /// encryption.
    ///
    /// A sync needs to be received to update the local room state. This method
    /// will wait for a sync to be received, this might time out if no
    /// sync loop is running or if the server is slow.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.enable_encryption().await?
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn enable_encryption(&self) -> Result<()> {
        use ruma::{
            events::room::encryption::RoomEncryptionEventContent, EventEncryptionAlgorithm,
        };
        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        if !self.is_encrypted().await? {
            let content =
                RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
            self.send_state_event(content).await?;

            // TODO do we want to return an error here if we time out? This
            // could be quite useful if someone wants to enable encryption and
            // send a message right after it's enabled.
            self.client.inner.sync_beat.listen().wait_timeout(SYNC_WAIT_TIME);
        }

        Ok(())
    }

    /// Share a room key with users in the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a room key that can be shared with them.
    ///
    /// Does nothing if no room key needs to be shared.
    // TODO: expose this publicly so people can pre-share a group session if
    // e.g. a user starts to type a message for a room.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all, fields(room_id = ?self.room_id()))]
    async fn preshare_room_key(&self) -> Result<()> {
        let mut map = self.client.inner.group_session_locks.lock().await;

        if let Some(mutex) = map.get(self.inner.room_id()).cloned() {
            // If a group session share request is already going on, await the
            // release of the lock.
            drop(map);
            _ = mutex.lock().await;
        } else {
            // Otherwise create a new lock and share the group
            // session.
            let mutex = Arc::new(Mutex::new(()));
            map.insert(self.inner.room_id().to_owned(), mutex.clone());

            drop(map);

            let _guard = mutex.lock().await;

            {
                let joined = self.client.store().get_joined_user_ids(self.inner.room_id()).await?;
                let invited =
                    self.client.store().get_invited_user_ids(self.inner.room_id()).await?;
                let members = joined.iter().chain(&invited).map(Deref::deref);
                self.client.claim_one_time_keys(members).await?;
            };

            let response = self.share_room_key().await;

            self.client.inner.group_session_locks.lock().await.remove(self.inner.room_id());

            // If one of the responses failed invalidate the group
            // session as using it would end up in undecryptable
            // messages.
            if let Err(r) = response {
                if let Some(machine) = self.client.olm_machine() {
                    machine.invalidate_group_session(self.inner.room_id()).await?;
                }
                return Err(r);
            }
        }

        Ok(())
    }

    /// Share a group session for a room.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    async fn share_room_key(&self) -> Result<()> {
        let requests = self.client.base_client().share_room_key(self.inner.room_id()).await?;

        for request in requests {
            let response = self.client.send_to_device(&request).await?;

            self.client.mark_request_as_sent(&request.txn_id, &response).await?;
        }

        Ok(())
    }

    /// Wait for the room to be fully synced.
    ///
    /// This method makes sure the room that was returned when joining a room
    /// has been echoed back in the sync.
    /// Warning: This waits until a sync happens and does not return if no sync
    /// is happening! It can also return early when the room is not a joined
    /// room anymore!
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn sync_up(&self) {
        while !self.is_synced() && self.state() == RoomState::Joined {
            self.client.inner.sync_beat.listen().wait_timeout(Duration::from_secs(1));
        }
    }

    /// Send a room message to this room.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if this room is encrypted.
    ///
    /// **Note**: If you just want to send a custom JSON payload to a room, you
    /// can use the [`Joined::send_raw()`] method for that.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    ///
    /// * `txn_id` - A locally-unique ID describing a message transaction with
    ///   the homeserver. Unless you're doing something special, you can pass in
    ///   `None` which will create a suitable one for you automatically.
    ///     * On the sending side, this field is used for re-trying earlier
    ///       failed transactions. Subsequent messages *must never* re-use an
    ///       earlier transaction ID.
    ///     * On the receiving side, the field is used for recognizing our own
    ///       messages when they arrive down the sync: the server includes the
    ///       ID in the [`MessageLikeUnsigned`] field [`transaction_id`] of the
    ///       corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///       device. Other devices will not see it. This is then used to ignore
    ///       events sent by our own device and/or to implement local echo.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::ruma::room_id;
    /// # use serde::{Deserialize, Serialize};
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::message::{RoomMessageEventContent, TextMessageEventContent},
    ///     },
    ///     uint, MilliSecondsSinceUnixEpoch, TransactionId,
    /// };
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    ///
    /// let content = RoomMessageEventContent::text_plain("Hello world");
    /// let txn_id = TransactionId::new();
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send(content, Some(&txn_id)).await?;
    /// }
    ///
    /// // Custom events work too:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.shiny_new_2fa.token", kind = MessageLike)]
    /// struct TokenEventContent {
    ///     token: String,
    ///     #[serde(rename = "exp")]
    ///     expires_at: MilliSecondsSinceUnixEpoch,
    /// }
    ///
    /// # fn generate_token() -> String { todo!() }
    /// let content = TokenEventContent {
    ///     token: generate_token(),
    ///     expires_at: {
    ///         let now = MilliSecondsSinceUnixEpoch::now();
    ///         MilliSecondsSinceUnixEpoch(now.0 + uint!(30_000))
    ///     },
    /// };
    /// let txn_id = TransactionId::new();
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send(content, Some(&txn_id)).await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    /// [`MessageLikeUnsigned`]: ruma::events::MessageLikeUnsigned
    /// [`transaction_id`]: ruma::events::MessageLikeUnsigned#structfield.transaction_id
    pub async fn send(
        &self,
        content: impl MessageLikeEventContent,
        txn_id: Option<&TransactionId>,
    ) -> Result<send_message_event::v3::Response> {
        let event_type = content.event_type().to_string();
        let content = serde_json::to_value(&content)?;

        self.send_raw(content, &event_type, txn_id).await
    }

    /// Send a room message to this room from a json `Value`.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if this room is encrypted.
    ///
    /// This method is equivalent to the [`Joined::send()`] method but allows
    /// sending custom JSON payloads, e.g. constructed using the
    /// [`serde_json::json!()`] macro.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the event as a json `Value`.
    ///
    /// * `event_type` - The type of the event.
    ///
    /// * `txn_id` - A locally-unique ID describing a message transaction with
    ///   the homeserver. Unless you're doing something special, you can pass in
    ///   `None` which will create a suitable one for you automatically.
    ///     * On the sending side, this field is used for re-trying earlier
    ///       failed transactions. Subsequent messages *must never* re-use an
    ///       earlier transaction ID.
    ///     * On the receiving side, the field is used for recognizing our own
    ///       messages when they arrive down the sync: the server includes the
    ///       ID in the [`StateUnsigned`] field [`transaction_id`] of the
    ///       corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///       device. Other devices will not see it. This is then used to ignore
    ///       events sent by our own device and/or to implement local echo.
    ///
    /// # Example
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::ruma::room_id;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// use serde_json::json;
    ///
    /// let content = json!({
    ///     "body": "Hello world",
    /// });
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_raw(content, "m.room.message", None).await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    /// [`StateUnsigned`]: ruma::events::StateUnsigned
    /// [`transaction_id`]: ruma::events::StateUnsigned#structfield.transaction_id
    pub async fn send_raw(
        &self,
        content: Value,
        event_type: &str,
        txn_id: Option<&TransactionId>,
    ) -> Result<send_message_event::v3::Response> {
        let txn_id: OwnedTransactionId = txn_id.map_or_else(TransactionId::new, ToOwned::to_owned);

        #[cfg(not(feature = "e2e-encryption"))]
        let content = {
            debug!(
                room_id = ?self.room_id(),
                "Sending plaintext event to room because we don't have encryption support.",
            );
            Raw::new(&content)?.cast()
        };

        #[cfg(feature = "e2e-encryption")]
        let (content, event_type) = if self.is_encrypted().await? {
            // Reactions are currently famously not encrypted, skip encrypting
            // them until they are.
            if event_type == "m.reaction" {
                debug!(
                    room_id = ?self.room_id(),
                    "Sending plaintext event because the event type is {event_type}",
                );
                (Raw::new(&content)?.cast(), event_type)
            } else {
                debug!(
                    room_id = self.room_id().as_str(),
                    "Sending encrypted event because the room is encrypted.",
                );

                if !self.are_members_synced() {
                    self.ensure_members().await?;
                    // TODO query keys here?
                }

                self.preshare_room_key().await?;

                let olm = self.client.olm_machine().expect("Olm machine wasn't started");

                let encrypted_content =
                    olm.encrypt_room_event_raw(self.inner.room_id(), content, event_type).await?;

                (encrypted_content.cast(), "m.room.encrypted")
            }
        } else {
            debug!(
                room_id = ?self.room_id(),
                "Sending plaintext event because the room is NOT encrypted.",
            );

            (Raw::new(&content)?.cast(), event_type)
        };

        let request = send_message_event::v3::Request::new_raw(
            self.inner.room_id().to_owned(),
            txn_id,
            event_type.into(),
            content,
        );

        let response = self.client.send(request, None).await?;
        Ok(response)
    }

    /// Send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`](#method.upload) method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`Client::upload()`](#Client::method.upload) and afterwards the
    /// [`send()`](#method.send).
    ///
    /// # Arguments
    /// * `body` - A textual representation of the media that is going to be
    /// uploaded. Usually the file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id, attachment::AttachmentConfig};
    /// # use url::Url;
    /// # use mime;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let mut image = fs::read("/home/example/my-cat.jpg")?;
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_attachment(
    ///         "My favorite cat",
    ///         &mime::IMAGE_JPEG,
    ///         image,
    ///         AttachmentConfig::new(),
    ///     ).await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn send_attachment(
        &self,
        body: &str,
        content_type: &Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> Result<send_message_event::v3::Response> {
        if config.thumbnail.is_some() {
            self.prepare_and_send_attachment(body, content_type, data, config).await
        } else {
            #[cfg(not(feature = "image-proc"))]
            let thumbnail = None;

            #[cfg(feature = "image-proc")]
            let data_slot;
            #[cfg(feature = "image-proc")]
            let (data, thumbnail) = if config.generate_thumbnail {
                let content_type = content_type.clone();
                let make_thumbnail = move |data| {
                    let res = generate_image_thumbnail(
                        &content_type,
                        Cursor::new(&data),
                        config.thumbnail_size,
                    );
                    (data, res)
                };

                #[cfg(not(target_arch = "wasm32"))]
                let (data, res) = tokio::task::spawn_blocking(move || make_thumbnail(data))
                    .await
                    .expect("Task join error");

                #[cfg(target_arch = "wasm32")]
                let (data, res) = make_thumbnail(data);

                let thumbnail = match res {
                    Ok((thumbnail_data, thumbnail_info)) => {
                        data_slot = thumbnail_data;
                        Some(Thumbnail {
                            data: data_slot,
                            content_type: mime::IMAGE_JPEG,
                            info: Some(thumbnail_info),
                        })
                    }
                    Err(
                        ImageError::ThumbnailBiggerThanOriginal | ImageError::FormatNotSupported,
                    ) => None,
                    Err(error) => return Err(error.into()),
                };

                (data, thumbnail)
            } else {
                (data, None)
            };

            let config = AttachmentConfig {
                txn_id: config.txn_id,
                info: config.info,
                thumbnail,
                #[cfg(feature = "image-proc")]
                generate_thumbnail: false,
                #[cfg(feature = "image-proc")]
                thumbnail_size: None,
            };

            self.prepare_and_send_attachment(body, content_type, data, config).await
        }
    }

    /// Prepare and send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`](#method.upload) method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`Client::upload()`](#Client::method.upload) and afterwards the
    /// [`send()`](#method.send).
    ///
    /// # Arguments
    /// * `body` - A textual representation of the media that is going to be
    /// uploaded. Usually the file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    async fn prepare_and_send_attachment(
        &self,
        body: &str,
        content_type: &Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> Result<send_message_event::v3::Response> {
        #[cfg(feature = "e2e-encryption")]
        let content = if self.is_encrypted().await? {
            self.client
                .prepare_encrypted_attachment_message(
                    body,
                    content_type,
                    data,
                    config.info,
                    config.thumbnail,
                )
                .await?
        } else {
            self.client
                .media()
                .prepare_attachment_message(body, content_type, data, config.info, config.thumbnail)
                .await?
        };

        #[cfg(not(feature = "e2e-encryption"))]
        let content = self
            .client
            .media()
            .prepare_attachment_message(body, content_type, data, config.info, config.thumbnail)
            .await?;

        self.send(RoomMessageEventContent::new(content), config.txn_id.as_deref()).await
    }

    /// Update the power levels of a select set of users of this room.
    ///
    /// Issue a `power_levels` state event request to the server, changing the
    /// given UserId -> Int levels. May fail if the `power_levels` aren't
    /// locally known yet or the server rejects the state event update, e.g.
    /// because of insufficient permissions. Neither permissions to update
    /// nor whether the data might be stale is checked prior to issuing the
    /// request.
    pub async fn update_power_levels(
        &self,
        updates: Vec<(&UserId, Int)>,
    ) -> Result<send_state_event::v3::Response> {
        let raw_pl_event = self
            .get_state_event_static::<RoomPowerLevelsEventContent>()
            .await?
            .ok_or(Error::InsufficientData)?;

        let mut power_levels = raw_pl_event.deserialize()?.power_levels();

        for (user_id, new_level) in updates {
            if new_level == power_levels.users_default {
                power_levels.users.remove(user_id);
            } else {
                power_levels.users.insert(user_id.to_owned(), new_level);
            }
        }

        self.send_state_event(RoomPowerLevelsEventContent::from(power_levels)).await
    }

    /// Send a state event with an empty state key to the homeserver.
    ///
    /// For state events with a non-empty state key, see
    /// [`send_state_event_for_key`][Self::send_state_event_for_key].
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::room::Joined = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent, room::encryption::RoomEncryptionEventContent,
    ///         EmptyStateKey,
    ///     },
    ///     EventEncryptionAlgorithm,
    /// };
    ///
    /// let encryption_event_content = RoomEncryptionEventContent::new(
    ///     EventEncryptionAlgorithm::MegolmV1AesSha2,
    /// );
    /// joined_room.send_state_event(encryption_event_content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(
    ///     type = "org.matrix.msc_9000.xxx",
    ///     kind = State,
    ///     state_key_type = EmptyStateKey,
    /// )]
    /// struct XxxStateEventContent {/* fields... */}
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event(content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn send_state_event(
        &self,
        content: impl StateEventContent<StateKey = EmptyStateKey>,
    ) -> Result<send_state_event::v3::Response> {
        self.send_state_event_for_key(&EmptyStateKey, content).await
    }

    /// Send a state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    ///   this piece of room state.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::room::Joined = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::member::{RoomMemberEventContent, MembershipState},
    ///     },
    ///     mxc_uri,
    /// };
    ///
    /// let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    /// let mut content = RoomMemberEventContent::new(MembershipState::Join);
    /// content.avatar_url = Some(avatar_url);
    ///
    /// joined_room.send_state_event_for_key(ruma::user_id!("@foo:bar.com"), content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.matrix.msc_9000.xxx", kind = State, state_key_type = String)]
    /// struct XxxStateEventContent { /* fields... */ }
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event_for_key("foo", content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn send_state_event_for_key<C, K>(
        &self,
        state_key: &K,
        content: C,
    ) -> Result<send_state_event::v3::Response>
    where
        C: StateEventContent,
        C::StateKey: Borrow<K>,
        K: AsRef<str> + ?Sized,
    {
        let request = send_state_event::v3::Request::new(
            self.inner.room_id().to_owned(),
            state_key,
            &content,
        )?;
        let response = self.client.send(request, None).await?;
        Ok(response)
    }

    /// Send a raw room state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The raw content of the state event.
    ///
    /// * `event_type` - The type of the event that we're sending out.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use serde_json::json;
    ///
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// let content = json!({
    ///     "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
    ///     "displayname": "Alice Margatroid",
    ///     "membership": "join"
    /// });
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_state_event_raw(content, "m.room.member", "").await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn send_state_event_raw(
        &self,
        content: Value,
        event_type: &str,
        state_key: &str,
    ) -> Result<send_state_event::v3::Response> {
        let content = Raw::new(&content)?.cast();
        let request = send_state_event::v3::Request::new_raw(
            self.inner.room_id().to_owned(),
            event_type.into(),
            state_key.to_owned(),
            content,
        );

        Ok(self.client.send(request, None).await?)
    }

    /// Strips all information out of an event of the room.
    ///
    /// Returns the [`redact_event::v3::Response`] from the server.
    ///
    /// This cannot be undone. Users may redact their own events, and any user
    /// with a power level greater than or equal to the redact power level of
    /// the room may redact events there.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted.
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID. If not given one is created for the message.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// use matrix_sdk::ruma::event_id;
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     let event_id = event_id!("$xxxxxx:example.org");
    ///     let reason = Some("Indecent material");
    ///     room.redact(&event_id, reason, None).await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    #[instrument(skip_all, parent = &self.client.inner.root_span)]
    pub async fn redact(
        &self,
        event_id: &EventId,
        reason: Option<&str>,
        txn_id: Option<OwnedTransactionId>,
    ) -> HttpResult<redact_event::v3::Response> {
        let txn_id = txn_id.unwrap_or_else(TransactionId::new);
        let request = assign!(
            redact_event::v3::Request::new(self.inner.room_id().to_owned(), event_id.to_owned(), txn_id),
            { reason: reason.map(ToOwned::to_owned) }
        );

        self.client.send(request, None).await
    }
}

/// Receipts to send all at once.
#[derive(Debug, Clone, Default)]
pub struct Receipts {
    fully_read: Option<OwnedEventId>,
    read_receipt: Option<OwnedEventId>,
    private_read_receipt: Option<OwnedEventId>,
}

impl Receipts {
    /// Create an empty `Receipts`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the last event the user has read.
    ///
    /// It means that the user has read all the events before this event.
    ///
    /// This is a private marker only visible by the user.
    ///
    /// Note that this is technically not a receipt as it is persisted in the
    /// room account data.
    pub fn fully_read_marker(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.fully_read = event_id.into();
        self
    }

    /// Set the last event presented to the user and forward it to the other
    /// users in the room.
    ///
    /// This is used to reset the unread messages/notification count and
    /// advertise to other users the last event that the user has likely seen.
    pub fn public_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.read_receipt = event_id.into();
        self
    }

    /// Set the last event presented to the user and don't forward it.
    ///
    /// This is used to reset the unread messages/notification count.
    pub fn private_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.private_read_receipt = event_id.into();
        self
    }

    /// Whether this `Receipts` is empty.
    pub fn is_empty(&self) -> bool {
        self.fully_read.is_none()
            && self.read_receipt.is_none()
            && self.private_read_receipt.is_none()
    }
}
