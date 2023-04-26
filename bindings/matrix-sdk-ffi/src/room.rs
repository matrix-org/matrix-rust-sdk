use std::{
    convert::TryFrom,
    sync::{Arc, RwLock},
};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use matrix_sdk::{
    room::{timeline::Timeline, Receipts, Room as SdkRoom},
    ruma::{
        api::client::{receipt::create_receipt::v3::ReceiptType, room::report_content},
        events::{
            reaction::ReactionEventContent,
            receipt::ReceiptThread,
            relation::{Annotation, Replacement},
            room::message::{
                ForwardThread, MessageType, Relation, RoomMessageEvent, RoomMessageEventContent,
            },
        },
        EventId, UserId,
    },
    RoomMemberships,
};
use mime::Mime;
use tracing::error;

use super::RUNTIME;
use crate::{error::ClientError, RoomMember, TimelineDiff, TimelineItem, TimelineListener};

#[derive(uniffi::Enum)]
pub enum Membership {
    Invited,
    Joined,
    Left,
}

pub(crate) type TimelineLock = Arc<RwLock<Option<Arc<Timeline>>>>;

#[derive(uniffi::Object)]
pub struct Room {
    room: SdkRoom,
    timeline: TimelineLock,
}

impl Room {
    pub(crate) fn new(room: SdkRoom) -> Self {
        Room { room, timeline: Default::default() }
    }

    pub(crate) fn with_timeline(room: SdkRoom, timeline: TimelineLock) -> Self {
        Room { room, timeline }
    }
}

#[uniffi::export]
impl Room {
    pub fn id(&self) -> String {
        self.room.room_id().to_string()
    }

    pub fn name(&self) -> Option<String> {
        self.room.name()
    }

    pub fn topic(&self) -> Option<String> {
        self.room.topic()
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.room.avatar_url().map(|m| m.to_string())
    }

    pub fn is_direct(&self) -> bool {
        RUNTIME.block_on(async move { self.room.is_direct().await.unwrap_or(false) })
    }

    pub fn is_public(&self) -> bool {
        self.room.is_public()
    }

    pub fn is_space(&self) -> bool {
        self.room.is_space()
    }

    pub fn is_tombstoned(&self) -> bool {
        self.room.is_tombstoned()
    }

    pub fn canonical_alias(&self) -> Option<String> {
        self.room.canonical_alias().map(|a| a.to_string())
    }

    pub fn alternative_aliases(&self) -> Vec<String> {
        self.room.alt_aliases().iter().map(|a| a.to_string()).collect()
    }

    pub fn membership(&self) -> Membership {
        match &self.room {
            SdkRoom::Invited(_) => Membership::Invited,
            SdkRoom::Joined(_) => Membership::Joined,
            SdkRoom::Left(_) => Membership::Left,
        }
    }

    pub fn inviter(&self) -> Option<Arc<RoomMember>> {
        match &self.room {
            SdkRoom::Invited(i) => RUNTIME.block_on(async move {
                i.invite_details()
                    .await
                    .ok()
                    .and_then(|a| a.inviter)
                    .map(|m| Arc::new(RoomMember::new(m)))
            }),
            SdkRoom::Joined(_) => None,
            SdkRoom::Left(_) => None,
        }
    }

    /// Removes the timeline.
    ///
    /// Timeline items cached in memory as well as timeline listeners are
    /// dropped.
    pub fn remove_timeline(&self) {
        *self.timeline.write().unwrap() = None;
    }

    pub fn retry_decryption(&self, session_ids: Vec<String>) {
        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't retry decryption");
                return;
            }
        };

        RUNTIME.spawn(async move {
            timeline.retry_decryption(&session_ids).await;
        });
    }

    pub fn fetch_members(&self) {
        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't fetch members");
                return;
            }
        };

        RUNTIME.spawn(async move {
            timeline.fetch_members().await;
        });
    }

    pub fn display_name(&self) -> Result<String, ClientError> {
        let r = self.room.clone();
        RUNTIME.block_on(async move { Ok(r.display_name().await?.to_string()) })
    }

    pub fn is_encrypted(&self) -> Result<bool, ClientError> {
        let room = self.room.clone();
        RUNTIME.block_on(async move {
            let is_encrypted = room.is_encrypted().await?;
            Ok(is_encrypted)
        })
    }

    pub fn members(&self) -> Result<Vec<Arc<RoomMember>>, ClientError> {
        let room = self.room.clone();
        RUNTIME.block_on(async move {
            let members = room
                .members(RoomMemberships::empty())
                .await?
                .iter()
                .map(|m| Arc::new(RoomMember::new(m.clone())))
                .collect();
            Ok(members)
        })
    }

    pub fn member_avatar_url(&self, user_id: String) -> Result<Option<String>, ClientError> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(user_id).await?.context("No user found")?;
            let avatar_url_string = member.avatar_url().map(|m| m.to_string());
            Ok(avatar_url_string)
        })
    }

    pub fn member_display_name(&self, user_id: String) -> Result<Option<String>, ClientError> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(user_id).await?.context("No user found")?;
            let avatar_url_string = member.display_name().map(|m| m.to_owned());
            Ok(avatar_url_string)
        })
    }

    pub fn add_timeline_listener(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> Vec<Arc<TimelineItem>> {
        let timeline = self
            .timeline
            .write()
            .unwrap()
            .get_or_insert_with(|| {
                let room = self.room.clone();
                #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
                let timeline = RUNTIME.block_on(room.timeline());
                Arc::new(timeline)
            })
            .clone();

        RUNTIME.block_on(async move {
            let (timeline_items, timeline_stream) = timeline.subscribe().await;

            let listener: Arc<dyn TimelineListener> = listener.into();
            RUNTIME.spawn(timeline_stream.for_each(move |diff| {
                let listener = listener.clone();
                let fut = RUNTIME
                    .spawn_blocking(move || listener.on_update(Arc::new(TimelineDiff::new(diff))));

                async move {
                    if let Err(e) = fut.await {
                        error!("Timeline listener error: {e}");
                    }
                }
            }));

            timeline_items.into_iter().map(TimelineItem::from_arc).collect()
        })
    }

    /// Loads older messages into the timeline.
    ///
    /// Raises an exception if there are no timeline listeners.
    pub fn paginate_backwards(&self, opts: PaginationOptions) -> Result<(), ClientError> {
        if let Some(timeline) = &*self.timeline.read().unwrap() {
            RUNTIME.block_on(async move { Ok(timeline.paginate_backwards(opts.into()).await?) })
        } else {
            Err(anyhow!("No timeline listeners registered, can't paginate").into())
        }
    }

    pub fn send_read_receipt(&self, event_id: String) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(
                    anyhow!("Can't send read receipts to room that isn't in joined state").into()
                )
            }
        };

        let event_id = EventId::parse(event_id)?;

        RUNTIME.block_on(async move {
            room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                .await?;
            Ok(())
        })
    }

    pub fn send_read_marker(
        &self,
        fully_read_event_id: String,
        read_receipt_event_id: Option<String>,
    ) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(
                    anyhow!("Can't send read markers to room that isn't in joined state").into()
                )
            }
        };

        let fully_read =
            EventId::parse(fully_read_event_id).context("parsing fully read event ID")?;
        let read_receipt = read_receipt_event_id
            .map(EventId::parse)
            .transpose()
            .context("parsing read receipt event ID")?;
        let receipts =
            Receipts::new().fully_read_marker(fully_read).public_read_receipt(read_receipt);

        RUNTIME.block_on(async move {
            room.send_multiple_receipts(receipts).await?;
            Ok(())
        })
    }

    pub fn send(&self, msg: Arc<RoomMessageEventContent>, txn_id: Option<String>) {
        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't send message");
                return;
            }
        };

        RUNTIME.spawn(async move {
            timeline.send((*msg).to_owned().into(), txn_id.as_deref().map(Into::into)).await;
        });
    }

    pub fn send_reply(
        &self,
        msg: String,
        in_reply_to_event_id: String,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => return Err(anyhow!("Can't send to a room that isn't in joined state").into()),
        };

        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => return Err(anyhow!("Timeline not set up, can't send message").into()),
        };

        let event_id: &EventId =
            in_reply_to_event_id.as_str().try_into().context("Failed to create EventId.")?;

        let reply_content = RUNTIME.block_on(async move {
            let timeline_event = room.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialize event")?;

            let original_message =
                event_content.as_original().context("Couldn't retrieve original message.")?;

            anyhow::Ok(
                RoomMessageEventContent::text_markdown(msg)
                    .make_reply_to(original_message, ForwardThread::Yes),
            )
        })?;

        RUNTIME.spawn(async move {
            timeline.send(reply_content.into(), txn_id.as_deref().map(Into::into)).await;
        });
        Ok(())
    }

    pub fn edit(
        &self,
        new_msg: String,
        original_event_id: String,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => return Err(anyhow!("Can't send to a room that isn't in joined state").into()),
        };

        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => return Err(anyhow!("Timeline not set up, can't send message").into()),
        };

        let event_id: &EventId =
            original_event_id.as_str().try_into().context("Failed to create EventId.")?;

        let edited_content = RUNTIME.block_on(async move {
            let timeline_event = room.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialise event")?;

            if self.own_user_id() != event_content.sender() {
                bail!("Can't edit an event not sent by own user");
            }

            let replacement = Replacement::new(
                event_id.to_owned(),
                MessageType::text_markdown(new_msg.to_owned()),
            );

            let mut edited_content = RoomMessageEventContent::text_markdown(new_msg);
            edited_content.relates_to = Some(Relation::Replacement(replacement));
            Ok(edited_content)
        })?;

        RUNTIME.spawn(async move {
            timeline.send(edited_content.into(), txn_id.as_deref().map(Into::into)).await;
        });
        Ok(())
    }

    /// Redacts an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted (optional).
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID (optional). If not given one is created.
    pub fn redact(
        &self,
        event_id: String,
        reason: Option<String>,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => return Err(anyhow!("Can't redact in a room that isn't in joined state").into()),
        };

        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            room.redact(&event_id, reason.as_deref(), txn_id.map(Into::into)).await?;
            Ok(())
        })
    }

    pub fn send_reaction(&self, event_id: String, key: String) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(
                    anyhow!("Can't send reaction in a room that isn't in joined state").into()
                )
            }
        };

        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            room.send(ReactionEventContent::new(Annotation::new(event_id, key)), None).await?;
            Ok(())
        })
    }

    /// Reports an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to report
    ///
    /// * `reason` - The reason for the event being reported (optional).
    ///
    /// * `score` - The score to rate this content as where -100 is most
    ///   offensive and 0 is inoffensive (optional).
    pub fn report_content(
        &self,
        event_id: String,
        score: Option<i32>,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let int_score = score.map(|value| value.into());
        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            self.room
                .client()
                .send(
                    report_content::v3::Request::new(
                        self.room_id().into(),
                        event_id,
                        int_score,
                        reason,
                    ),
                    None,
                )
                .await?;
            Ok(())
        })
    }

    /// Ignores a user.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the user to ignore.
    pub fn ignore_user(&self, user_id: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(user_id)?;
            self.client().account().ignore_user(&user_id).await?;
            Ok(())
        })
    }

    /// Leaves the joined room.
    ///
    /// Will throw an error if used on an room that isn't in a joined state
    pub fn leave(&self) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => return Err(anyhow!("Can't leave a room that isn't in joined state").into()),
        };

        RUNTIME.block_on(async move {
            room.leave().await?;
            Ok(())
        })
    }

    /// Rejects the invitation for the invited room.
    ///
    /// Will throw an error if used on an room that isn't in an invited state
    pub fn reject_invitation(&self) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Invited(i) => i.clone(),
            _ => {
                return Err(anyhow!(
                    "Can't reject an invite for a room that isn't in invited state"
                )
                .into())
            }
        };

        RUNTIME.block_on(async move {
            room.reject_invitation().await?;
            Ok(())
        })
    }

    /// Accepts the invitation for the invited room.
    ///
    /// Will throw an error if used on an room that isn't in an invited state
    pub fn accept_invitation(&self) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Invited(i) => i.clone(),
            _ => {
                return Err(anyhow!(
                    "Can't accept an invite for a room that isn't in invited state"
                )
                .into())
            }
        };

        RUNTIME.block_on(async move {
            room.accept_invitation().await?;
            Ok(())
        })
    }

    /// Sets a new topic in the room.
    pub fn set_topic(&self, topic: String) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(anyhow!("Can't set a topic in a room that isn't in joined state").into())
            }
        };

        RUNTIME.block_on(async move {
            room.set_room_topic(&topic).await?;
            Ok(())
        })
    }

    /// Upload and set the room's avatar.
    ///
    /// This will upload the data produced by the reader to the homeserver's
    /// content repository, and set the room's avatar to the MXC URI for the
    /// uploaded file.
    ///
    /// # Arguments
    ///
    /// * `mime_type` - The mime description of the avatar, for example
    ///   image/jpeg
    /// * `data` - The raw data that will be uploaded to the homeserver's
    ///   content repository
    pub fn upload_avatar(&self, mime_type: String, data: Vec<u8>) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(
                    anyhow!("Can't set a avatar in a room that isn't in joined state").into()
                )
            }
        };

        RUNTIME.block_on(async move {
            let mime: Mime = mime_type.parse()?;
            // TODO: We could add an FFI ImageInfo struct in the future
            room.upload_avatar(&mime, data, None).await?;
            Ok(())
        })
    }

    /// Removes the current room avatar
    pub fn remove_avatar(&self) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => {
                return Err(
                    anyhow!("Can't remove a avatar in a room that isn't in joined state").into()
                )
            }
        };

        RUNTIME.block_on(async move {
            room.remove_avatar().await?;
            Ok(())
        })
    }

    pub fn invite_user_by_id(&self, user_id: String) -> Result<(), ClientError> {
        let room = match &self.room {
            SdkRoom::Joined(joined_room) => joined_room.clone(),
            _ => return Err(anyhow!("Can't invite user to room that isn't in joined state").into()),
        };

        RUNTIME.block_on(async move {
            let user = <&UserId>::try_from(user_id.as_str())
                .context("Could not create user from string")?;
            room.invite_user_by_id(user).await?;
            Ok(())
        })
    }

    pub fn fetch_event_details(&self, event_id: String) -> Result<(), ClientError> {
        let timeline = self
            .timeline
            .read()
            .unwrap()
            .as_ref()
            .context("Timeline not set up, can't fetch event details")?
            .clone();

        RUNTIME.block_on(async move {
            let event_id = <&EventId>::try_from(event_id.as_str())?;
            timeline.fetch_event_details(event_id).await.context("Fetching event details")?;
            Ok(())
        })
    }
}

impl std::ops::Deref for Room {
    type Target = SdkRoom;

    fn deref(&self) -> &SdkRoom {
        &self.room
    }
}

pub enum PaginationOptions {
    SingleRequest { event_limit: u16 },
    UntilNumItems { event_limit: u16, items: u16 },
}

impl From<PaginationOptions> for matrix_sdk::room::timeline::PaginationOptions<'static> {
    fn from(value: PaginationOptions) -> Self {
        use matrix_sdk::room::timeline::PaginationOptions as Opts;
        match value {
            PaginationOptions::SingleRequest { event_limit } => Opts::single_request(event_limit),
            PaginationOptions::UntilNumItems { event_limit, items } => {
                Opts::until_num_items(event_limit, items)
            }
        }
    }
}
