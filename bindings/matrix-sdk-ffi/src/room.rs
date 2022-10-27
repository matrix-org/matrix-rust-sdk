use std::{
    convert::TryFrom,
    sync::{Arc, RwLock},
};

use anyhow::{bail, Context, Result};
use futures_signals::signal_vec::SignalVecExt;
use matrix_sdk::{
    room::{
        timeline::{PaginationOutcome, Timeline},
        Room as SdkRoom,
    },
    ruma::{
        events::room::message::{RoomMessageEvent, RoomMessageEventContent},
        EventId, UserId,
    },
};
use tracing::error;

use super::RUNTIME;
use crate::{TimelineDiff, TimelineListener};

#[derive(uniffi::Enum)]
pub enum Membership {
    Invited,
    Joined,
    Left,
}

pub struct Room {
    room: SdkRoom,
    timeline: RwLock<Option<Arc<Timeline>>>,
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
        self.room.is_direct()
    }

    pub fn is_public(&self) -> bool {
        self.room.is_public()
    }

    pub fn is_encrypted(&self) -> bool {
        self.room.is_encrypted()
    }

    pub fn is_space(&self) -> bool {
        self.room.is_space()
    }

    pub fn is_tombstoned(&self) -> bool {
        self.room.is_tombstoned()
    }

    pub fn membership(&self) -> Membership {
        match &self.room {
            SdkRoom::Invited(_) => Membership::Invited,
            SdkRoom::Joined(_) => Membership::Joined,
            SdkRoom::Left(_) => Membership::Left,
        }
    }

    /// Removes the timeline.
    ///
    /// Timeline items cached in memory as well as timeline listeners are
    /// dropped.
    pub fn remove_timeline(&self) {
        *self.timeline.write().unwrap() = None;
    }
}

impl Room {
    pub fn new(room: SdkRoom) -> Self {
        Room { room, timeline: RwLock::default() }
    }

    pub fn display_name(&self) -> Result<String> {
        let r = self.room.clone();
        RUNTIME.block_on(async move { Ok(r.display_name().await?.to_string()) })
    }

    pub fn member_avatar_url(&self, user_id: String) -> Result<Option<String>> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(user_id).await?.context("No user found")?;
            let avatar_url_string = member.avatar_url().map(|m| m.to_string());
            Ok(avatar_url_string)
        })
    }

    pub fn member_display_name(&self, user_id: String) -> Result<Option<String>> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(user_id).await?.context("No user found")?;
            let avatar_url_string = member.display_name().map(|m| m.to_owned());
            Ok(avatar_url_string)
        })
    }

    pub fn add_timeline_listener(&self, listener: Box<dyn TimelineListener>) {
        let timeline_signal = self
            .timeline
            .write()
            .unwrap()
            .get_or_insert_with(|| Arc::new(self.room.timeline()))
            .signal();

        let listener: Arc<dyn TimelineListener> = listener.into();
        RUNTIME.spawn(timeline_signal.for_each(move |diff| {
            let listener = listener.clone();
            let fut = RUNTIME
                .spawn_blocking(move || listener.on_update(Arc::new(TimelineDiff::new(diff))));

            async move {
                if let Err(e) = fut.await {
                    error!("Timeline listener error: {e}");
                }
            }
        }));
    }

    pub fn paginate_backwards(&self, limit: u16) -> Result<PaginationOutcome> {
        if let Some(timeline) = &*self.timeline.read().unwrap() {
            RUNTIME.block_on(async move { Ok(timeline.paginate_backwards(limit.into()).await?) })
        } else {
            bail!("No timeline listeners registered, can't paginate");
        }
    }

    pub fn send(&self, msg: Arc<RoomMessageEventContent>, txn_id: Option<String>) -> Result<()> {
        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => bail!("Timeline not set up, can't send message"),
        };

        RUNTIME.block_on(async move {
            timeline.send((*msg).to_owned().into(), txn_id.as_deref().map(Into::into)).await?;
            Ok(())
        })
    }

    pub fn send_reply(
        &self,
        msg: String,
        in_reply_to_event_id: String,
        txn_id: Option<String>,
    ) -> Result<()> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => bail!("Can't send to a room that isn't in joined state"),
        };

        let timeline = match &*self.timeline.read().unwrap() {
            Some(t) => Arc::clone(t),
            None => bail!("Timeline not set up, can't send message"),
        };

        let event_id: &EventId =
            in_reply_to_event_id.as_str().try_into().context("Failed to create EventId.")?;

        RUNTIME.block_on(async move {
            let timeline_event = room.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialise event")?;

            let original_message =
                event_content.as_original().context("Couldn't retrieve original message.")?;

            let reply_content =
                RoomMessageEventContent::text_markdown(msg).make_reply_to(original_message);

            timeline.send(reply_content.into(), txn_id.as_deref().map(Into::into)).await?;

            Ok(())
        })
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
    ) -> Result<()> {
        let room = match &self.room {
            SdkRoom::Joined(j) => j.clone(),
            _ => bail!("Can't redact in a room that isn't in joined state"),
        };

        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            room.redact(&event_id, reason.as_deref(), txn_id.map(Into::into)).await?;
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
