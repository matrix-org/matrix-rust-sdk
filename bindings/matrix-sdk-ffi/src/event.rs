use anyhow::{bail, Context};
use ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};

use crate::{ClientError, MessageType};

pub struct TimelineEvent(pub(crate) AnySyncTimelineEvent);

#[uniffi::export]
impl TimelineEvent {
    pub fn event_id(&self) -> String {
        self.0.event_id().to_string()
    }

    pub fn sender_id(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn event_type(&self) -> Result<TimelineEventType, ClientError> {
        let event_type = match &self.0 {
            AnySyncTimelineEvent::MessageLike(event) => {
                TimelineEventType::MessageLike { content: event.clone().try_into()? }
            }
            AnySyncTimelineEvent::State(_) => TimelineEventType::State,
        };
        Ok(event_type)
    }
}

impl TryFrom<AnySyncMessageLikeEvent> for MessageLikeEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncMessageLikeEvent) -> anyhow::Result<Self> {
        let content = match value {
            AnySyncMessageLikeEvent::CallAnswer(_) => MessageLikeEventContent::CallAnswer,
            AnySyncMessageLikeEvent::CallInvite(_) => MessageLikeEventContent::CallInvite,
            AnySyncMessageLikeEvent::CallHangup(_) => MessageLikeEventContent::CallHangup,
            AnySyncMessageLikeEvent::CallCandidates(_) => MessageLikeEventContent::CallCandidates,
            AnySyncMessageLikeEvent::KeyVerificationReady(_) => {
                MessageLikeEventContent::KeyVerificationReady
            }
            AnySyncMessageLikeEvent::KeyVerificationStart(_) => {
                MessageLikeEventContent::KeyVerificationStart
            }
            AnySyncMessageLikeEvent::KeyVerificationCancel(_) => {
                MessageLikeEventContent::KeyVerificationCancel
            }
            AnySyncMessageLikeEvent::KeyVerificationAccept(_) => {
                MessageLikeEventContent::KeyVerificationAccept
            }
            AnySyncMessageLikeEvent::KeyVerificationKey(_) => {
                MessageLikeEventContent::KeyVerificationKey
            }
            AnySyncMessageLikeEvent::KeyVerificationMac(_) => {
                MessageLikeEventContent::KeyVerificationMac
            }
            AnySyncMessageLikeEvent::KeyVerificationDone(_) => {
                MessageLikeEventContent::KeyVerificationDone
            }
            AnySyncMessageLikeEvent::Reaction(_) => MessageLikeEventContent::Reaction,
            AnySyncMessageLikeEvent::RoomEncrypted(_) => MessageLikeEventContent::RoomEncrypted,
            AnySyncMessageLikeEvent::RoomMessage(content) => {
                let original_content = content
                    .as_original()
                    .context("Failed to get original content")?
                    .content
                    .clone();
                MessageLikeEventContent::RoomMessage {
                    message_type: original_content.msgtype.into(),
                }
            }
            AnySyncMessageLikeEvent::RoomRedaction(_) => MessageLikeEventContent::RoomRedaction,
            AnySyncMessageLikeEvent::Sticker(_) => MessageLikeEventContent::Sticker,
            _ => bail!("Unsupported Event Type"),
        };
        Ok(content)
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineEventType {
    MessageLike { content: MessageLikeEventContent },
    // we don't support state events yet so they do not have any content
    State,
}

#[derive(uniffi::Enum)]
pub enum MessageLikeEventContent {
    CallAnswer,
    CallInvite,
    CallHangup,
    CallCandidates,
    KeyVerificationReady,
    KeyVerificationStart,
    KeyVerificationCancel,
    KeyVerificationAccept,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationDone,
    Reaction,
    RoomEncrypted,
    RoomMessage { message_type: MessageType },
    RoomRedaction,
    Sticker,
}
