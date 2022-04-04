#[cfg(feature = "image-proc")]
use std::io::Cursor;
#[cfg(feature = "encryption")]
use std::sync::Arc;
use std::{
    io::{BufReader, Read, Seek},
    ops::Deref,
};

use matrix_sdk_common::instant::{Duration, Instant};
#[cfg(feature = "encryption")]
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
        receipt::create_receipt,
        redact::redact_event,
        state::send_state_event,
        typing::create_typing_event::v3::{Request as TypingRequest, Typing},
    },
    assign,
    events::{
        room::message::RoomMessageEventContent, EventContent, MessageLikeEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, TransactionId, UserId,
};
use serde_json::Value;
use tracing::debug;
#[cfg(feature = "encryption")]
use tracing::instrument;

#[cfg(feature = "image-proc")]
use crate::{attachment::generate_image_thumbnail, error::ImageError};
use crate::{
    attachment::{AttachmentConfig, Thumbnail},
    error::HttpResult,
    room::Common,
    BaseRoom, Client, Result, RoomType,
};

const TYPING_NOTICE_TIMEOUT: Duration = Duration::from_secs(4);
const TYPING_NOTICE_RESEND_TIMEOUT: Duration = Duration::from_secs(3);

/// A room in the joined state.
///
/// The `JoinedRoom` contains all methods specific to a `Room` with type
/// `RoomType::Joined`. Operations may fail once the underlying `Room` changes
/// `RoomType`.
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
    /// Create a new `room::Joined` if the underlying `BaseRoom` has type
    /// `RoomType::Joined`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub fn new(client: Client, room: BaseRoom) -> Option<Self> {
        // TODO: Make this private
        if room.room_type() == RoomType::Joined {
            Some(Self { inner: Common::new(client, room) })
        } else {
            None
        }
    }

    /// Leave this room.
    pub async fn leave(&self) -> Result<()> {
        self.inner.leave().await
    }

    /// Ban the user with `UserId` from this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user to ban with `UserId`.
    ///
    /// * `reason` - The reason for banning this user.
    pub async fn ban_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request =
            assign!(ban_user::v3::Request::new(self.inner.room_id(), user_id), { reason });
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
    pub async fn kick_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request =
            assign!(kick_user::v3::Request::new(self.inner.room_id(), user_id), { reason });
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Invite the specified user by `UserId` to this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user to invite to the room.
    pub async fn invite_user_by_id(&self, user_id: &UserId) -> Result<()> {
        let recipient = InvitationRecipient::UserId { user_id };

        let request = invite_user::v3::Request::new(self.inner.room_id(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Invite the specified user by third party id to this room.
    ///
    /// # Arguments
    ///
    /// * `invite_id` - A third party id of a user to invite to the room.
    pub async fn invite_user_by_3pid(&self, invite_id: Invite3pid<'_>) -> Result<()> {
        let recipient = InvitationRecipient::ThirdPartyId(invite_id);
        let request = invite_user::v3::Request::new(self.inner.room_id(), recipient);
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
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
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

            let request =
                TypingRequest::new(self.inner.own_user_id(), self.inner.room_id(), typing);
            self.client.send(request, None).await?;
        }

        Ok(())
    }

    /// Send a request to notify this room that the user has read specific
    /// event.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The `EventId` specifies the event to set the read receipt
    ///   on.
    pub async fn read_receipt(&self, event_id: &EventId) -> Result<()> {
        let request =
            create_receipt::v3::Request::new(self.inner.room_id(), ReceiptType::Read, event_id);

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Send a request to notify this room that the user has read up to specific
    /// event.
    ///
    /// # Arguments
    ///
    /// * fully_read - The `EventId` of the event the user has read to.
    ///
    /// * read_receipt - An `EventId` to specify the event to set the read
    ///   receipt on.
    pub async fn read_marker(
        &self,
        fully_read: &EventId,
        read_receipt: Option<&EventId>,
    ) -> Result<()> {
        let request =
            assign!(set_read_marker::v3::Request::new(self.inner.room_id(), fully_read), {
                read_receipt
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
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn enable_encryption(&self) -> Result<()> {
        use ruma::{
            events::room::encryption::RoomEncryptionEventContent, EventEncryptionAlgorithm,
        };
        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        if !self.is_encrypted() {
            let content =
                RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
            self.send_state_event(content, "").await?;

            // TODO do we want to return an error here if we time out? This
            // could be quite useful if someone wants to enable encryption and
            // send a message right after it's enabled.
            self.client.inner.sync_beat.listen().wait_timeout(SYNC_WAIT_TIME);
        }

        Ok(())
    }

    /// Share a group session for the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a group session with them.
    ///
    /// Does nothing if no group session needs to be shared.
    #[cfg(feature = "encryption")]
    async fn preshare_group_session(&self) -> Result<()> {
        // TODO expose this publicly so people can pre-share a group session if
        // e.g. a user starts to type a message for a room.
        if let Some(mutex) =
            self.client.inner.group_session_locks.get(self.inner.room_id()).map(|m| m.clone())
        {
            // If a group session share request is already going on,
            // await the release of the lock.
            mutex.lock().await;
        } else {
            // Otherwise create a new lock and share the group
            // session.
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .inner
                .group_session_locks
                .insert(self.inner.room_id().to_owned(), mutex.clone());

            let _guard = mutex.lock().await;

            {
                let joined = self.client.store().get_joined_user_ids(self.inner.room_id()).await?;
                let invited =
                    self.client.store().get_invited_user_ids(self.inner.room_id()).await?;
                let members = joined.iter().chain(&invited).map(Deref::deref);
                self.client.claim_one_time_keys(members).await?;
            };

            let response = self.share_group_session().await;

            self.client.inner.group_session_locks.remove(self.inner.room_id());

            // If one of the responses failed invalidate the group
            // session as using it would end up in undecryptable
            // messages.
            if let Err(r) = response {
                self.client.base_client().invalidate_group_session(self.inner.room_id()).await?;
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
    #[instrument]
    #[cfg(feature = "encryption")]
    async fn share_group_session(&self) -> Result<()> {
        let requests = self.client.base_client().share_group_session(self.inner.room_id()).await?;

        for request in requests {
            let response = self.client.send_to_device(&request).await?;

            self.client.mark_request_as_sent(&request.txn_id, &response).await?;
        }

        Ok(())
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
    /// # use std::convert::TryFrom;
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
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    ///
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    /// [`MessageLikeUnsigned`]: ruma::events::MessageLikeUnsigned
    /// [`transaction_id`]: ruma::events::MessageLikeUnsigned#structfield.transaction_id
    pub async fn send(
        &self,
        content: impl EventContent<EventType = MessageLikeEventType>,
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
    /// # use std::convert::TryFrom;
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
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
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
        let txn_id: Box<TransactionId> = txn_id.map_or_else(TransactionId::new, ToOwned::to_owned);

        #[cfg(not(feature = "encryption"))]
        let content = {
            debug!(
                room_id = self.room_id().as_str(),
                "Sending plaintext event to room because we don't have encryption support.",
            );
            Raw::new(&content)?.cast()
        };

        #[cfg(feature = "encryption")]
        let (content, event_type) = if self.is_encrypted() {
            debug!(
                room_id = self.room_id().as_str(),
                "Sending encrypted event because the room is encrypted.",
            );

            if !self.are_members_synced() {
                self.request_members().await?;
                // TODO query keys here?
            }

            self.preshare_group_session().await?;

            let olm = self.client.olm_machine().await.expect("Olm machine wasn't started");

            let encrypted_content =
                olm.encrypt_raw(self.inner.room_id(), content, event_type).await?;
            let raw_content =
                Raw::new(&encrypted_content).expect("Failed to serialize encrypted event").cast();

            (raw_content, "m.room.encrypted")
        } else {
            debug!(
                room_id = self.room_id().as_str(),
                "Sending plaintext event because the room is NOT encrypted.",
            );

            (Raw::new(&content)?.cast(), event_type)
        };

        let request = send_message_event::v3::Request::new_raw(
            self.inner.room_id(),
            &txn_id,
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
    /// # use std::{path::PathBuf, fs::File, io::Read};
    /// # use matrix_sdk::{Client, ruma::room_id, attachment::AttachmentConfig};
    /// # use url::Url;
    /// # use mime;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let path = PathBuf::from("/home/example/my-cat.jpg");
    /// let mut image = File::open(path)?;
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_attachment(
    ///         "My favorite cat",
    ///         &mime::IMAGE_JPEG,
    ///         &mut image,
    ///         AttachmentConfig::new(),
    ///     ).await?;
    /// }
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn send_attachment<R: Read + Seek, T: Read>(
        &self,
        body: &str,
        content_type: &Mime,
        reader: &mut R,
        config: AttachmentConfig<'_, T>,
    ) -> Result<send_message_event::v3::Response> {
        let reader = &mut BufReader::new(reader);

        #[cfg(feature = "image-proc")]
        let mut cursor;

        if config.thumbnail.is_some() {
            self.prepare_and_send_attachment(body, content_type, reader, config).await
        } else {
            #[cfg(not(feature = "image-proc"))]
            let thumbnail = Thumbnail::NONE;

            #[cfg(feature = "image-proc")]
            let thumbnail = if config.generate_thumbnail {
                match generate_image_thumbnail(content_type, reader, config.thumbnail_size) {
                    Ok((thumbnail_data, thumbnail_info)) => {
                        reader.rewind()?;

                        cursor = Cursor::new(thumbnail_data);
                        Some(Thumbnail {
                            reader: &mut cursor,
                            content_type: &mime::IMAGE_JPEG,
                            info: Some(thumbnail_info),
                        })
                    }
                    Err(
                        ImageError::ThumbnailBiggerThanOriginal | ImageError::FormatNotSupported,
                    ) => {
                        reader.rewind()?;
                        None
                    }
                    Err(error) => return Err(error.into()),
                }
            } else {
                None
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

            self.prepare_and_send_attachment(body, content_type, reader, config).await
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
    async fn prepare_and_send_attachment<R: Read, T: Read>(
        &self,
        body: &str,
        content_type: &Mime,
        reader: &mut R,
        config: AttachmentConfig<'_, T>,
    ) -> Result<send_message_event::v3::Response> {
        #[cfg(feature = "encryption")]
        let content = if self.is_encrypted() {
            self.client
                .prepare_encrypted_attachment_message(
                    body,
                    content_type,
                    reader,
                    config.info,
                    config.thumbnail,
                )
                .await?
        } else {
            self.client
                .prepare_attachment_message(
                    body,
                    content_type,
                    reader,
                    config.info,
                    config.thumbnail,
                )
                .await?
        };

        #[cfg(not(feature = "encryption"))]
        let content = self
            .client
            .prepare_attachment_message(body, content_type, reader, config.info, config.thumbnail)
            .await?;

        self.send(RoomMessageEventContent::new(content), config.txn_id).await
    }

    /// Send a room state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::member::{RoomMemberEventContent, MembershipState},
    ///     },
    ///     assign, mxc_uri,
    /// };
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    ///
    /// let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    /// let content = assign!(RoomMemberEventContent::new(MembershipState::Join), {
    ///    avatar_url: Some(avatar_url),
    /// });
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_state_event(content, "").await?;
    /// }
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.matrix.msc_9000.xxx", kind = State)]
    /// struct XxxStateEventContent { /* fields... */ }
    /// let content: XxxStateEventContent = todo!();
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     room.send_state_event(content, "").await?;
    /// }
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn send_state_event(
        &self,
        content: impl EventContent<EventType = StateEventType>,
        state_key: &str,
    ) -> Result<send_state_event::v3::Response> {
        let request =
            send_state_event::v3::Request::new(self.inner.room_id(), state_key, &content)?;
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
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    pub async fn send_state_event_raw(
        &self,
        content: Value,
        event_type: &str,
        state_key: &str,
    ) -> Result<send_state_event::v3::Response> {
        let content = Raw::new(&content)?.cast();
        let request = send_state_event::v3::Request::new_raw(
            self.inner.room_id(),
            event_type.into(),
            state_key,
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
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn redact(
        &self,
        event_id: &EventId,
        reason: Option<&str>,
        txn_id: Option<Box<TransactionId>>,
    ) -> HttpResult<redact_event::v3::Response> {
        let txn_id = txn_id.unwrap_or_else(TransactionId::new);
        let request =
            assign!(redact_event::v3::Request::new(self.inner.room_id(), event_id, &txn_id), {
                reason
            });

        self.client.send(request, None).await
    }
}
