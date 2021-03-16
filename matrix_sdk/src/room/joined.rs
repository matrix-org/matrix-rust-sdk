use crate::{room::Common, Client, Result, Room, RoomType};
use std::{io::Read, ops::Deref, sync::Arc};

use matrix_sdk_common::{
    api::r0::{
        membership::{
            ban_user,
            invite_user::{self, InvitationRecipient},
            kick_user, Invite3pid,
        },
        message::send_message_event,
        read_marker::set_read_marker,
        receipt::create_receipt,
        redact::redact_event,
        state::send_state_event_for_key,
        typing::create_typing_event::{Request as TypingRequest, Typing},
    },
    assign,
    events::{
        room::{
            message::{
                AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
                MessageEventContent, MessageType, VideoMessageEventContent,
            },
            EncryptedFile,
        },
        AnyMessageEventContent, AnyStateEventContent,
    },
    identifiers::{EventId, UserId},
    uuid::Uuid,
};

use mime::{self, Mime};

#[cfg(feature = "encryption")]
use matrix_sdk_common::locks::Mutex;

#[cfg(feature = "encryption")]
use matrix_sdk_base::crypto::AttachmentEncryptor;

#[cfg(feature = "encryption")]
use tracing::instrument;

/// A room in the joined state.
///
/// The `JoinedRoom` contains all methodes specific to a `Room` with type `RoomType::Joined`.
/// Operations may fail once the underlaying `Room` changes `RoomType`.
#[derive(Debug, Clone)]
pub struct Joined {
    inner: Common,
}

impl Deref for Joined {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Joined {
    /// Create a new `room::Joined` if the underlaying `Room` has type `RoomType::Joined`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlaying room.
    pub fn new(client: Client, room: Room) -> Option<Self> {
        // TODO: Make this private
        if room.room_type() == RoomType::Joined {
            Some(Self {
                inner: Common::new(client, room),
            })
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
        let request = assign!(ban_user::Request::new(self.inner.room_id(), user_id), {
            reason
        });
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Kick a user out of this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user that should be kicked out of the room.
    ///
    /// * `reason` - Optional reason why the room member is being kicked out.
    pub async fn kick_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(kick_user::Request::new(self.inner.room_id(), user_id), {
            reason
        });
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

        let request = invite_user::Request::new(self.inner.room_id(), recipient);
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
        let request = invite_user::Request::new(self.inner.room_id(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Send a request to notify this room of a user typing.
    ///
    /// # Arguments
    ///
    /// * `typing` - Whether the user is typing, and how long.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use matrix_sdk::{
    /// #     Client, SyncSettings,
    /// #     api::r0::typing::create_typing_event::Typing,
    /// #     identifiers::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = room_id!("!test:localhost");
    /// # let room = client
    /// #    .get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost"))
    /// #    .unwrap();
    /// # room
    ///     .typing_notice(Typing::Yes(Duration::from_secs(4)))
    ///     .await
    ///     .expect("Can't get devices from server");
    /// # });
    ///
    /// ```
    pub async fn typing_notice(&self, typing: impl Into<Typing>) -> Result<()> {
        // TODO: don't send a request if a typing notice is being sent or is already active
        let request = TypingRequest::new(
            self.inner.own_user_id(),
            self.inner.room_id(),
            typing.into(),
        );

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Send a request to notify this room that the user has read specific event.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The `EventId` specifies the event to set the read receipt on.
    pub async fn read_receipt(&self, event_id: &EventId) -> Result<()> {
        let request = create_receipt::Request::new(
            self.inner.room_id(),
            create_receipt::ReceiptType::Read,
            event_id,
        );

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Send a request to notify this room that the user has read up to specific event.
    ///
    /// # Arguments
    ///
    /// * fully_read - The `EventId` of the event the user has read to.
    ///
    /// * read_receipt - An `EventId` to specify the event to set the read receipt on.
    pub async fn read_marker(
        &self,
        fully_read: &EventId,
        read_receipt: Option<&EventId>,
    ) -> Result<()> {
        let request = assign!(
            set_read_marker::Request::new(self.inner.room_id(), fully_read),
            { read_receipt }
        );

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Share a group session for the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a group session with them.
    ///
    /// Does nothing if no group session needs to be shared.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    async fn preshare_group_session(&self) -> Result<()> {
        // TODO expose this publicly so people can pre-share a group session if
        // e.g. a user starts to type a message for a room.
        #[allow(clippy::map_clone)]
        if let Some(mutex) = self
            .client
            .group_session_locks
            .get(self.inner.room_id())
            .map(|m| m.clone())
        {
            // If a group session share request is already going on,
            // await the release of the lock.
            mutex.lock().await;
        } else {
            // Otherwise create a new lock and share the group
            // session.
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .group_session_locks
                .insert(self.inner.room_id().clone(), mutex.clone());

            let _guard = mutex.lock().await;

            {
                let joined = self
                    .client
                    .store()
                    .get_joined_user_ids(self.inner.room_id())
                    .await?;
                let invited = self
                    .client
                    .store()
                    .get_invited_user_ids(self.inner.room_id())
                    .await?;
                let members = joined.iter().chain(&invited);
                self.client.claim_one_time_keys(members).await?;
            };

            let response = self.share_group_session().await;

            self.client.group_session_locks.remove(self.inner.room_id());

            // If one of the responses failed invalidate the group
            // session as using it would end up in undecryptable
            // messages.
            if let Err(r) = response {
                self.client
                    .base_client
                    .invalidate_group_session(self.inner.room_id())
                    .await;
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
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn share_group_session(&self) -> Result<()> {
        let mut requests = self
            .client
            .base_client
            .share_group_session(self.inner.room_id())
            .await?;

        for request in requests.drain(..) {
            let response = self.client.send_to_device(&request).await?;

            self.client
                .base_client
                .mark_request_as_sent(&request.txn_id, &response)
                .await?;
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
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    ///
    /// * `txn_id` - A unique `Uuid` that can be attached to a `MessageEvent`
    /// held in its unsigned field as `transaction_id`. If not given one is
    /// created for the message.
    ///
    /// # Example
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::identifiers::room_id;
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::events::{
    ///     AnyMessageEventContent,
    ///     room::message::{MessageEventContent, TextMessageEventContent},
    /// };
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = room_id!("!test:localhost");
    /// use matrix_sdk_common::uuid::Uuid;
    ///
    /// let content = AnyMessageEventContent::RoomMessage(
    ///     MessageEventContent::text_plain("Hello world")
    /// );
    ///
    /// let txn_id = Uuid::new_v4();
    /// # let room = client
    /// #    .get_joined_room(&room_id)
    /// #    .unwrap();
    /// # room.send(content, Some(txn_id)).await.unwrap();
    /// # })
    /// ```
    pub async fn send(
        &self,
        content: impl Into<AnyMessageEventContent>,
        txn_id: Option<Uuid>,
    ) -> Result<send_message_event::Response> {
        #[cfg(not(feature = "encryption"))]
        let content: AnyMessageEventContent = content.into();

        #[cfg(feature = "encryption")]
        let content = if self.is_encrypted() {
            if !self.are_members_synced() {
                self.request_members().await?;
                // TODO query keys here?
            }

            self.preshare_group_session().await?;
            AnyMessageEventContent::RoomEncrypted(
                self.client
                    .base_client
                    .encrypt(self.inner.room_id(), content)
                    .await?,
            )
        } else {
            content.into()
        };

        let txn_id = txn_id.unwrap_or_else(Uuid::new_v4).to_string();
        let request = send_message_event::Request::new(&self.inner.room_id(), &txn_id, &content);

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
    /// This is a convenience method that calls the [`Client::upload()`](#Client::method.upload)
    /// and afterwards the [`send()`](#method.send).
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
    /// * `txn_id` - A unique `Uuid` that can be attached to a `MessageEvent`
    /// held in its unsigned field as `transaction_id`. If not given one is
    /// created for the message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, fs::File, io::Read};
    /// # use matrix_sdk::{Client, identifiers::room_id};
    /// # use url::Url;
    /// # use mime;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = room_id!("!test:localhost");
    /// let path = PathBuf::from("/home/example/my-cat.jpg");
    /// let mut image = File::open(path).unwrap();
    ///
    /// # let room = client
    /// #    .get_joined_room(&room_id)
    /// #    .unwrap();
    /// # room.send_attachment("My favorite cat", &mime::IMAGE_JPEG, &mut image, None)
    ///     .await
    ///     .expect("Can't upload my cat.");
    /// # });
    /// ```
    pub async fn send_attachment<R: Read>(
        &self,
        body: &str,
        content_type: &Mime,
        mut reader: &mut R,
        txn_id: Option<Uuid>,
    ) -> Result<send_message_event::Response> {
        let (response, encrypted_file) = if self.is_encrypted() {
            #[cfg(feature = "encryption")]
            let mut reader = AttachmentEncryptor::new(reader);
            #[cfg(feature = "encryption")]
            let content_type = mime::APPLICATION_OCTET_STREAM;

            let response = self.client.upload(&content_type, &mut reader).await?;

            #[cfg(feature = "encryption")]
            let keys = {
                let keys = reader.finish();
                Some(Box::new(EncryptedFile {
                    url: response.content_uri.clone(),
                    key: keys.web_key,
                    iv: keys.iv,
                    hashes: keys.hashes,
                    v: keys.version,
                }))
            };
            #[cfg(not(feature = "encryption"))]
            let keys: Option<Box<EncryptedFile>> = None;

            (response, keys)
        } else {
            let response = self.client.upload(&content_type, &mut reader).await?;
            (response, None)
        };

        let url = response.content_uri;

        let content = match content_type.type_() {
            mime::IMAGE => {
                // TODO create a thumbnail using the image crate?.
                MessageType::Image(ImageMessageEventContent {
                    body: body.to_owned(),
                    info: None,
                    url: Some(url),
                    file: encrypted_file,
                })
            }
            mime::AUDIO => MessageType::Audio(AudioMessageEventContent {
                body: body.to_owned(),
                info: None,
                url: Some(url),
                file: encrypted_file,
            }),
            mime::VIDEO => MessageType::Video(VideoMessageEventContent {
                body: body.to_owned(),
                info: None,
                url: Some(url),
                file: encrypted_file,
            }),
            _ => MessageType::File(FileMessageEventContent {
                filename: None,
                body: body.to_owned(),
                info: None,
                url: Some(url),
                file: encrypted_file,
            }),
        };

        self.send(
            AnyMessageEventContent::RoomMessage(MessageEventContent::new(content)),
            txn_id,
        )
        .await
    }

    /// Send a room state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `room_id` -  The id of the room that should receive the message.
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::events::{
    ///     AnyStateEventContent,
    ///     room::member::{MemberEventContent, MembershipState},
    /// };
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = matrix_sdk::Client::new(homeserver).unwrap();
    /// # let room_id = matrix_sdk::identifiers::room_id!("!test:localhost");
    ///
    /// let avatar_url = "https://example.org/avatar";
    /// let member_event = MemberEventContent {
    ///    avatar_url: Some(avatar_url.to_string()),
    ///    membership: MembershipState::Join,
    ///    is_direct: None,
    ///    displayname: None,    
    ///    third_party_invite: None,
    /// };
    ///
    /// # let room = client
    /// #    .get_joined_room(&room_id)
    /// #    .unwrap();
    ///
    /// let content = AnyStateEventContent::RoomMember(member_event);
    /// room.send_state_event(content, "").await.unwrap();
    /// # })
    /// ```
    pub async fn send_state_event(
        &self,
        content: impl Into<AnyStateEventContent>,
        state_key: &str,
    ) -> Result<send_state_event_for_key::Response> {
        let content = content.into();
        let request =
            send_state_event_for_key::Request::new(self.inner.room_id(), state_key, &content);

        self.client.send(request, None).await
    }

    /// Strips all information out of an event of the room.
    ///
    /// Returns the [`redact_event::Response`] from the server.
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
    /// * `txn_id` - A unique [`Uuid`] that can be attached to this event as
    /// its transaction ID. If not given one is created for the message.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = matrix_sdk::Client::new(homeserver).unwrap();
    /// # let room_id = matrix_sdk::identifiers::room_id!("!test:localhost");
    /// # let room = client
    /// #   .get_joined_room(&room_id)
    /// #   .unwrap();
    /// let event_id = matrix_sdk::identifiers::event_id!("$xxxxxx:example.org");
    /// let reason = Some("Indecent material");
    /// room.redact(&event_id, reason, None).await.unwrap();
    /// # })
    /// ```
    pub async fn redact(
        &self,
        event_id: &EventId,
        reason: Option<&str>,
        txn_id: Option<Uuid>,
    ) -> Result<redact_event::Response> {
        let txn_id = txn_id.unwrap_or_else(Uuid::new_v4).to_string();
        let request = assign!(
            redact_event::Request::new(self.inner.room_id(), event_id, &txn_id),
            { reason }
        );

        self.client.send(request, None).await
    }
}
