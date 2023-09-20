use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::bail;
use as_variant::as_variant;
use extension_trait::extension_trait;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    attachment::{BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo},
    ruma::events::{
        location::AssetType as RumaAssetType,
        poll::start::PollKind as RumaPollKind,
        room::{
            message::{
                AudioInfo as RumaAudioInfo,
                AudioMessageEventContent as RumaAudioMessageEventContent,
                EmoteMessageEventContent as RumaEmoteMessageEventContent, FileInfo as RumaFileInfo,
                FileMessageEventContent as RumaFileMessageEventContent,
                FormattedBody as RumaFormattedBody,
                ImageMessageEventContent as RumaImageMessageEventContent,
                LocationMessageEventContent as RumaLocationMessageEventContent,
                MessageType as RumaMessageType,
                NoticeMessageEventContent as RumaNoticeMessageEventContent,
                RoomMessageEventContentWithoutRelation,
                TextMessageEventContent as RumaTextMessageEventContent,
                UnstableAudioDetailsContentBlock as RumaUnstableAudioDetailsContentBlock,
                UnstableVoiceContentBlock as RumaUnstableVoiceContentBlock,
                VideoInfo as RumaVideoInfo,
                VideoMessageEventContent as RumaVideoMessageEventContent,
            },
            ImageInfo as RumaImageInfo, MediaSource, ThumbnailInfo as RumaThumbnailInfo,
        },
    },
};
use matrix_sdk_ui::timeline::{EventItemOrigin, PollResult, Profile, TimelineDetails};
use ruma::{assign, UInt};
use tracing::{info, warn};

use crate::{
    error::{ClientError, MediaInfoError},
    helpers::unwrap_or_clone_arc,
    room::AssetType,
};

#[uniffi::export]
pub fn media_source_from_url(url: String) -> Arc<MediaSource> {
    Arc::new(MediaSource::Plain(url.into()))
}

#[uniffi::export]
pub fn message_event_content_new(
    msgtype: MessageType,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(msgtype.into()))
}

#[uniffi::export]
pub fn message_event_content_from_markdown(
    md: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::text_markdown(md)))
}

#[uniffi::export]
pub fn message_event_content_from_html(
    body: String,
    html_body: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::text_html(
        body, html_body,
    )))
}

#[uniffi::export(callback_interface)]
pub trait TimelineListener: Sync + Send {
    fn on_update(&self, diff: Vec<Arc<TimelineDiff>>);
}

#[derive(Clone, uniffi::Object)]
pub enum TimelineDiff {
    Append { values: Vec<Arc<TimelineItem>> },
    Clear,
    PushFront { value: Arc<TimelineItem> },
    PushBack { value: Arc<TimelineItem> },
    PopFront,
    PopBack,
    Insert { index: usize, value: Arc<TimelineItem> },
    Set { index: usize, value: Arc<TimelineItem> },
    Remove { index: usize },
    Truncate { length: usize },
    Reset { values: Vec<Arc<TimelineItem>> },
}

impl TimelineDiff {
    pub(crate) fn new(inner: VectorDiff<Arc<matrix_sdk_ui::timeline::TimelineItem>>) -> Self {
        match inner {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(TimelineItem::from_arc).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index, value: TimelineItem::from_arc(value) }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index, value: TimelineItem::from_arc(value) }
            }
            VectorDiff::Truncate { length } => Self::Truncate { length },
            VectorDiff::Remove { index } => Self::Remove { index },
            VectorDiff::PushBack { value } => {
                Self::PushBack { value: TimelineItem::from_arc(value) }
            }
            VectorDiff::PushFront { value } => {
                Self::PushFront { value: TimelineItem::from_arc(value) }
            }
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::Reset { values } => {
                warn!("Timeline subscriber lagged behind and was reset");
                Self::Reset { values: values.into_iter().map(TimelineItem::from_arc).collect() }
            }
        }
    }
}

#[uniffi::export]
impl TimelineDiff {
    pub fn change(&self) -> TimelineChange {
        match self {
            Self::Append { .. } => TimelineChange::Append,
            Self::Insert { .. } => TimelineChange::Insert,
            Self::Set { .. } => TimelineChange::Set,
            Self::Remove { .. } => TimelineChange::Remove,
            Self::PushBack { .. } => TimelineChange::PushBack,
            Self::PushFront { .. } => TimelineChange::PushFront,
            Self::PopBack => TimelineChange::PopBack,
            Self::PopFront => TimelineChange::PopFront,
            Self::Clear => TimelineChange::Clear,
            Self::Truncate { .. } => TimelineChange::Truncate,
            Self::Reset { .. } => TimelineChange::Reset,
        }
    }

    pub fn append(self: Arc<Self>) -> Option<Vec<Arc<TimelineItem>>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Append { values } => values)
    }

    pub fn insert(self: Arc<Self>) -> Option<InsertData> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Insert { index, value } => {
            InsertData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn set(self: Arc<Self>) -> Option<SetData> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Set { index, value } => {
            SetData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn remove(&self) -> Option<u32> {
        as_variant!(self, Self::Remove { index } => (*index).try_into().unwrap())
    }

    pub fn push_back(self: Arc<Self>) -> Option<Arc<TimelineItem>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::PushBack { value } => value)
    }

    pub fn push_front(self: Arc<Self>) -> Option<Arc<TimelineItem>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::PushFront { value } => value)
    }

    pub fn reset(self: Arc<Self>) -> Option<Vec<Arc<TimelineItem>>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Reset { values } => values)
    }
}

#[derive(uniffi::Record)]
pub struct InsertData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

#[derive(uniffi::Record)]
pub struct SetData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum TimelineChange {
    Append,
    Clear,
    Insert,
    Set,
    Remove,
    PushBack,
    PushFront,
    PopBack,
    PopFront,
    Truncate,
    Reset,
}

#[repr(transparent)]
#[derive(Clone, uniffi::Object)]
pub struct TimelineItem(pub(crate) matrix_sdk_ui::timeline::TimelineItem);

impl TimelineItem {
    pub(crate) fn from_arc(arc: Arc<matrix_sdk_ui::timeline::TimelineItem>) -> Arc<Self> {
        // SAFETY: This is valid because Self is a repr(transparent) wrapper
        //         around the other Timeline type.
        unsafe { Arc::from_raw(Arc::into_raw(arc) as _) }
    }
}

#[uniffi::export]
impl TimelineItem {
    pub fn as_event(self: Arc<Self>) -> Option<Arc<EventTimelineItem>> {
        let event_item = self.0.as_event()?;
        Some(Arc::new(EventTimelineItem(event_item.clone())))
    }

    pub fn as_virtual(self: Arc<Self>) -> Option<VirtualTimelineItem> {
        use matrix_sdk_ui::timeline::VirtualTimelineItem as VItem;
        match self.0.as_virtual()? {
            VItem::DayDivider(ts) => Some(VirtualTimelineItem::DayDivider { ts: ts.0.into() }),
            VItem::ReadMarker => Some(VirtualTimelineItem::ReadMarker),
        }
    }

    pub fn unique_id(&self) -> u64 {
        self.0.unique_id()
    }

    pub fn fmt_debug(&self) -> String {
        format!("{:#?}", self.0)
    }
}

/// This type represents the “send state” of a local event timeline item.
#[derive(Clone, uniffi::Enum)]
pub enum EventSendState {
    /// The local event has not been sent yet.
    NotSentYet,
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed { error: String },
    /// Sending has been cancelled because an earlier event in the
    /// message-sending queue failed.
    Cancelled,
    /// The local event has been sent successfully to the server.
    Sent { event_id: String },
}

impl From<&matrix_sdk_ui::timeline::EventSendState> for EventSendState {
    fn from(value: &matrix_sdk_ui::timeline::EventSendState) -> Self {
        use matrix_sdk_ui::timeline::EventSendState::*;

        match value {
            NotSentYet => Self::NotSentYet,
            SendingFailed { error } => Self::SendingFailed { error: error.to_string() },
            Cancelled => Self::Cancelled,
            Sent { event_id } => Self::Sent { event_id: event_id.to_string() },
        }
    }
}

#[derive(uniffi::Object)]
pub struct EventTimelineItem(pub(crate) matrix_sdk_ui::timeline::EventTimelineItem);

#[uniffi::export]
impl EventTimelineItem {
    pub fn is_local(&self) -> bool {
        self.0.is_local_echo()
    }

    pub fn is_remote(&self) -> bool {
        !self.0.is_local_echo()
    }

    pub fn transaction_id(&self) -> Option<String> {
        self.0.transaction_id().map(ToString::to_string)
    }

    pub fn event_id(&self) -> Option<String> {
        self.0.event_id().map(ToString::to_string)
    }

    pub fn sender(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn sender_profile(&self) -> ProfileDetails {
        self.0.sender_profile().into()
    }

    pub fn is_own(&self) -> bool {
        self.0.is_own()
    }

    pub fn is_editable(&self) -> bool {
        self.0.is_editable()
    }

    pub fn content(&self) -> Arc<TimelineItemContent> {
        Arc::new(TimelineItemContent(self.0.content().clone()))
    }

    pub fn timestamp(&self) -> u64 {
        self.0.timestamp().0.into()
    }

    pub fn reactions(&self) -> Vec<Reaction> {
        self.0
            .reactions()
            .iter()
            .map(|(k, v)| Reaction {
                key: k.to_owned(),
                count: v.len().try_into().unwrap(),
                senders: v
                    .senders()
                    .map(|v| ReactionSenderData {
                        sender_id: v.sender_id.to_string(),
                        timestamp: v.timestamp.0.into(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn debug_info(&self) -> EventTimelineItemDebugInfo {
        EventTimelineItemDebugInfo {
            model: format!("{:#?}", self.0),
            original_json: self.0.original_json().map(|raw| raw.json().get().to_owned()),
            latest_edit_json: self.0.latest_edit_json().map(|raw| raw.json().get().to_owned()),
        }
    }

    pub fn local_send_state(&self) -> Option<EventSendState> {
        self.0.send_state().map(Into::into)
    }

    pub fn read_receipts(&self) -> HashMap<String, Receipt> {
        self.0.read_receipts().iter().map(|(k, v)| (k.to_string(), v.clone().into())).collect()
    }

    pub fn origin(&self) -> Option<EventItemOrigin> {
        self.0.origin()
    }
}

#[derive(uniffi::Record)]
pub struct Receipt {
    pub timestamp: Option<u64>,
}

impl From<ruma::events::receipt::Receipt> for Receipt {
    fn from(value: ruma::events::receipt::Receipt) -> Self {
        Receipt { timestamp: value.ts.map(|ts| ts.0.into()) }
    }
}

#[derive(uniffi::Record)]
pub struct EventTimelineItemDebugInfo {
    model: String,
    original_json: Option<String>,
    latest_edit_json: Option<String>,
}

#[derive(uniffi::Enum)]
pub enum ProfileDetails {
    Unavailable,
    Pending,
    Ready { display_name: Option<String>, display_name_ambiguous: bool, avatar_url: Option<String> },
    Error { message: String },
}

impl From<&TimelineDetails<Profile>> for ProfileDetails {
    fn from(details: &TimelineDetails<Profile>) -> Self {
        match details {
            TimelineDetails::Unavailable => Self::Unavailable,
            TimelineDetails::Pending => Self::Pending,
            TimelineDetails::Ready(profile) => Self::Ready {
                display_name: profile.display_name.clone(),
                display_name_ambiguous: profile.display_name_ambiguous,
                avatar_url: profile.avatar_url.as_ref().map(ToString::to_string),
            },
            TimelineDetails::Error(e) => Self::Error { message: e.to_string() },
        }
    }
}

#[derive(Clone, uniffi::Object)]
pub struct TimelineItemContent(matrix_sdk_ui::timeline::TimelineItemContent);

#[uniffi::export]
impl TimelineItemContent {
    pub fn kind(&self) -> TimelineItemContentKind {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;

        match &self.0 {
            Content::Message(_) => TimelineItemContentKind::Message,
            Content::RedactedMessage => TimelineItemContentKind::RedactedMessage,
            Content::Sticker(sticker) => {
                let content = sticker.content();
                TimelineItemContentKind::Sticker {
                    body: content.body.clone(),
                    info: (&content.info).into(),
                    url: content.url.to_string(),
                }
            }
            Content::Poll(poll_state) => TimelineItemContentKind::from(poll_state.results()),
            Content::UnableToDecrypt(msg) => {
                TimelineItemContentKind::UnableToDecrypt { msg: EncryptedMessage::new(msg) }
            }
            Content::MembershipChange(membership) => TimelineItemContentKind::RoomMembership {
                user_id: membership.user_id().to_string(),
                change: membership.change().map(Into::into),
            },
            Content::ProfileChange(profile) => {
                let (display_name, prev_display_name) = profile
                    .displayname_change()
                    .map(|change| (change.new.clone(), change.old.clone()))
                    .unzip();
                let (avatar_url, prev_avatar_url) = profile
                    .avatar_url_change()
                    .map(|change| {
                        (
                            change.new.as_ref().map(ToString::to_string),
                            change.old.as_ref().map(ToString::to_string),
                        )
                    })
                    .unzip();
                TimelineItemContentKind::ProfileChange {
                    display_name: display_name.flatten(),
                    prev_display_name: prev_display_name.flatten(),
                    avatar_url: avatar_url.flatten(),
                    prev_avatar_url: prev_avatar_url.flatten(),
                }
            }
            Content::OtherState(state) => TimelineItemContentKind::State {
                state_key: state.state_key().to_owned(),
                content: state.content().into(),
            },
            Content::FailedToParseMessageLike { event_type, error } => {
                TimelineItemContentKind::FailedToParseMessageLike {
                    event_type: event_type.to_string(),
                    error: error.to_string(),
                }
            }
            Content::FailedToParseState { event_type, state_key, error } => {
                TimelineItemContentKind::FailedToParseState {
                    event_type: event_type.to_string(),
                    state_key: state_key.to_string(),
                    error: error.to_string(),
                }
            }
        }
    }

    pub fn as_message(self: Arc<Self>) -> Option<Arc<Message>> {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;
        unwrap_or_clone_arc_into_variant!(self, .0, Content::Message(msg) => Arc::new(Message(msg)))
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineItemContentKind {
    Message,
    RedactedMessage,
    Sticker {
        body: String,
        info: ImageInfo,
        url: String,
    },
    Poll {
        question: String,
        kind: PollKind,
        max_selections: u64,
        answers: Vec<PollAnswer>,
        votes: HashMap<String, Vec<String>>,
        end_time: Option<u64>,
    },
    UnableToDecrypt {
        msg: EncryptedMessage,
    },
    RoomMembership {
        user_id: String,
        change: Option<MembershipChange>,
    },
    ProfileChange {
        display_name: Option<String>,
        prev_display_name: Option<String>,
        avatar_url: Option<String>,
        prev_avatar_url: Option<String>,
    },
    State {
        state_key: String,
        content: OtherState,
    },
    FailedToParseMessageLike {
        event_type: String,
        error: String,
    },
    FailedToParseState {
        event_type: String,
        state_key: String,
        error: String,
    },
}

#[derive(Clone, uniffi::Object)]
pub struct Message(matrix_sdk_ui::timeline::Message);

#[uniffi::export]
impl Message {
    pub fn msgtype(&self) -> Option<MessageType> {
        self.0.msgtype().clone().try_into().ok()
    }

    pub fn body(&self) -> String {
        self.0.msgtype().body().to_owned()
    }

    pub fn in_reply_to(&self) -> Option<InReplyToDetails> {
        self.0.in_reply_to().map(InReplyToDetails::from)
    }

    pub fn is_threaded(&self) -> bool {
        self.0.is_threaded()
    }

    pub fn is_edited(&self) -> bool {
        self.0.is_edited()
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MessageType {
    Emote { content: EmoteMessageContent },
    Image { content: ImageMessageContent },
    Audio { content: AudioMessageContent },
    Video { content: VideoMessageContent },
    File { content: FileMessageContent },
    Notice { content: NoticeMessageContent },
    Text { content: TextMessageContent },
    Location { content: LocationContent },
}

impl From<MessageType> for RumaMessageType {
    fn from(value: MessageType) -> Self {
        match value {
            MessageType::Emote { content } => {
                Self::Emote(assign!(RumaEmoteMessageEventContent::plain(content.body), {
                    formatted: content.formatted.map(Into::into),
                }))
            }
            MessageType::Image { content } => Self::Image(
                RumaImageMessageEventContent::new(content.body, (*content.source).clone())
                    .info(content.info.map(Into::into).map(Box::new)),
            ),
            MessageType::Audio { content } => Self::Audio(
                RumaAudioMessageEventContent::new(content.body, (*content.source).clone())
                    .info(content.info.map(Into::into).map(Box::new)),
            ),
            MessageType::Video { content } => Self::Video(
                RumaVideoMessageEventContent::new(content.body, (*content.source).clone())
                    .info(content.info.map(Into::into).map(Box::new)),
            ),
            MessageType::File { content } => Self::File(
                RumaFileMessageEventContent::new(content.body, (*content.source).clone())
                    .filename(content.filename)
                    .info(content.info.map(Into::into).map(Box::new)),
            ),
            MessageType::Notice { content } => {
                Self::Notice(assign!(RumaNoticeMessageEventContent::plain(content.body), {
                    formatted: content.formatted.map(Into::into),
                }))
            }
            MessageType::Text { content } => {
                Self::Text(assign!(RumaTextMessageEventContent::plain(content.body), {
                    formatted: content.formatted.map(Into::into),
                }))
            }
            MessageType::Location { content } => {
                Self::Location(RumaLocationMessageEventContent::new(content.body, content.geo_uri))
            }
        }
    }
}

impl TryFrom<RumaMessageType> for MessageType {
    type Error = anyhow::Error;

    fn try_from(value: RumaMessageType) -> anyhow::Result<Self> {
        let message_type = match value {
            RumaMessageType::Emote(c) => MessageType::Emote {
                content: EmoteMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            },
            RumaMessageType::Image(c) => MessageType::Image {
                content: ImageMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            },
            RumaMessageType::Audio(c) => MessageType::Audio {
                content: AudioMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                    audio: c.audio.map(Into::into),
                    voice: c.voice.map(Into::into),
                },
            },
            RumaMessageType::Video(c) => MessageType::Video {
                content: VideoMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            },
            RumaMessageType::File(c) => MessageType::File {
                content: FileMessageContent {
                    body: c.body.clone(),
                    filename: c.filename.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            },
            RumaMessageType::Notice(c) => MessageType::Notice {
                content: NoticeMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            },
            RumaMessageType::Text(c) => MessageType::Text {
                content: TextMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            },
            RumaMessageType::Location(c) => {
                let (description, zoom_level) =
                    c.location.map(|loc| (loc.description, loc.zoom_level)).unwrap_or((None, None));
                MessageType::Location {
                    content: LocationContent {
                        body: c.body,
                        geo_uri: c.geo_uri,
                        description,
                        zoom_level: zoom_level.and_then(|z| z.get().try_into().ok()),
                        asset: c.asset.and_then(|a| match a.type_ {
                            RumaAssetType::Self_ => Some(AssetType::Sender),
                            RumaAssetType::Pin => Some(AssetType::Pin),
                            _ => None,
                        }),
                    },
                }
            }
            _ => bail!("Unsupported type"),
        };
        Ok(message_type)
    }
}

#[derive(Clone, uniffi::Record)]
pub struct EmoteMessageContent {
    pub body: String,
    pub formatted: Option<FormattedBody>,
}

#[derive(Clone, uniffi::Record)]
pub struct ImageMessageContent {
    pub body: String,
    pub source: Arc<MediaSource>,
    pub info: Option<ImageInfo>,
}

#[derive(Clone, uniffi::Record)]
pub struct AudioMessageContent {
    pub body: String,
    pub source: Arc<MediaSource>,
    pub info: Option<AudioInfo>,
    pub audio: Option<UnstableAudioDetailsContent>,
    pub voice: Option<UnstableVoiceContent>,
}

#[derive(Clone, uniffi::Record)]
pub struct VideoMessageContent {
    pub body: String,
    pub source: Arc<MediaSource>,
    pub info: Option<VideoInfo>,
}

#[derive(Clone, uniffi::Record)]
pub struct FileMessageContent {
    pub body: String,
    pub filename: Option<String>,
    pub source: Arc<MediaSource>,
    pub info: Option<FileInfo>,
}

#[derive(Clone, uniffi::Record)]
pub struct ImageInfo {
    pub height: Option<u64>,
    pub width: Option<u64>,
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub thumbnail_info: Option<ThumbnailInfo>,
    pub thumbnail_source: Option<Arc<MediaSource>>,
    pub blurhash: Option<String>,
}

impl From<ImageInfo> for RumaImageInfo {
    fn from(value: ImageInfo) -> Self {
        assign!(RumaImageInfo::new(), {
            height: value.height.map(u64_to_uint),
            width: value.width.map(u64_to_uint),
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
            thumbnail_info: value.thumbnail_info.map(Into::into).map(Box::new),
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone()),
            blurhash: value.blurhash,
        })
    }
}

impl TryFrom<&ImageInfo> for BaseImageInfo {
    type Error = MediaInfoError;

    fn try_from(value: &ImageInfo) -> Result<Self, MediaInfoError> {
        let height = UInt::try_from(value.height.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let width = UInt::try_from(value.width.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let size = UInt::try_from(value.size.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let blurhash = value.blurhash.clone().ok_or(MediaInfoError::MissingField)?;

        Ok(BaseImageInfo {
            height: Some(height),
            width: Some(width),
            size: Some(size),
            blurhash: Some(blurhash),
        })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct AudioInfo {
    pub duration: Option<Duration>,
    pub size: Option<u64>,
    pub mimetype: Option<String>,
}

impl From<AudioInfo> for RumaAudioInfo {
    fn from(value: AudioInfo) -> Self {
        assign!(RumaAudioInfo::new(), {
            duration: value.duration,
            size: value.size.map(u64_to_uint),
            mimetype: value.mimetype,
        })
    }
}

impl TryFrom<&AudioInfo> for BaseAudioInfo {
    type Error = MediaInfoError;

    fn try_from(value: &AudioInfo) -> Result<Self, MediaInfoError> {
        let duration = value.duration.ok_or(MediaInfoError::MissingField)?;
        let size = UInt::try_from(value.size.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;

        Ok(BaseAudioInfo { duration: Some(duration), size: Some(size) })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct UnstableAudioDetailsContent {
    pub duration: Duration,
    pub waveform: Vec<u16>,
}

impl From<RumaUnstableAudioDetailsContentBlock> for UnstableAudioDetailsContent {
    fn from(details: RumaUnstableAudioDetailsContentBlock) -> Self {
        Self {
            duration: details.duration,
            waveform: details
                .waveform
                .iter()
                .map(|x| u16::try_from(x.get()).unwrap_or(0))
                .collect(),
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct UnstableVoiceContent {}

impl From<RumaUnstableVoiceContentBlock> for UnstableVoiceContent {
    fn from(_details: RumaUnstableVoiceContentBlock) -> Self {
        Self {}
    }
}

#[derive(Clone, uniffi::Record)]
pub struct VideoInfo {
    pub duration: Option<Duration>,
    pub height: Option<u64>,
    pub width: Option<u64>,
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub thumbnail_info: Option<ThumbnailInfo>,
    pub thumbnail_source: Option<Arc<MediaSource>>,
    pub blurhash: Option<String>,
}

impl From<VideoInfo> for RumaVideoInfo {
    fn from(value: VideoInfo) -> Self {
        assign!(RumaVideoInfo::new(), {
            duration: value.duration,
            height: value.height.map(u64_to_uint),
            width: value.width.map(u64_to_uint),
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
            thumbnail_info: value.thumbnail_info.map(Into::into).map(Box::new),
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone()),
            blurhash: value.blurhash,
        })
    }
}

impl TryFrom<&VideoInfo> for BaseVideoInfo {
    type Error = MediaInfoError;

    fn try_from(value: &VideoInfo) -> Result<Self, MediaInfoError> {
        let duration = value.duration.ok_or(MediaInfoError::MissingField)?;
        let height = UInt::try_from(value.height.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let width = UInt::try_from(value.width.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let size = UInt::try_from(value.size.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let blurhash = value.blurhash.clone().ok_or(MediaInfoError::MissingField)?;

        Ok(BaseVideoInfo {
            duration: Some(duration),
            height: Some(height),
            width: Some(width),
            size: Some(size),
            blurhash: Some(blurhash),
        })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct FileInfo {
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub thumbnail_info: Option<ThumbnailInfo>,
    pub thumbnail_source: Option<Arc<MediaSource>>,
}

impl From<FileInfo> for RumaFileInfo {
    fn from(value: FileInfo) -> Self {
        assign!(RumaFileInfo::new(), {
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
            thumbnail_info: value.thumbnail_info.map(Into::into).map(Box::new),
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone()),
        })
    }
}

impl TryFrom<&FileInfo> for BaseFileInfo {
    type Error = MediaInfoError;

    fn try_from(value: &FileInfo) -> Result<Self, MediaInfoError> {
        let size = UInt::try_from(value.size.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;

        Ok(BaseFileInfo { size: Some(size) })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct ThumbnailInfo {
    pub height: Option<u64>,
    pub width: Option<u64>,
    pub mimetype: Option<String>,
    pub size: Option<u64>,
}

impl From<ThumbnailInfo> for RumaThumbnailInfo {
    fn from(value: ThumbnailInfo) -> Self {
        assign!(RumaThumbnailInfo::new(), {
            height: value.height.map(u64_to_uint),
            width: value.width.map(u64_to_uint),
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
        })
    }
}

impl TryFrom<&ThumbnailInfo> for BaseThumbnailInfo {
    type Error = MediaInfoError;

    fn try_from(value: &ThumbnailInfo) -> Result<Self, MediaInfoError> {
        let height = UInt::try_from(value.height.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let width = UInt::try_from(value.width.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;
        let size = UInt::try_from(value.size.ok_or(MediaInfoError::MissingField)?)
            .map_err(|_| MediaInfoError::InvalidField)?;

        Ok(BaseThumbnailInfo { height: Some(height), width: Some(width), size: Some(size) })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct NoticeMessageContent {
    pub body: String,
    pub formatted: Option<FormattedBody>,
}

#[derive(Clone, uniffi::Record)]
pub struct TextMessageContent {
    pub body: String,
    pub formatted: Option<FormattedBody>,
}

#[derive(Clone, uniffi::Record)]
pub struct LocationContent {
    pub body: String,
    pub geo_uri: String,
    pub description: Option<String>,
    pub zoom_level: Option<u8>,
    pub asset: Option<AssetType>,
}

#[derive(Clone, uniffi::Record)]
pub struct FormattedBody {
    pub format: MessageFormat,
    pub body: String,
}

impl From<FormattedBody> for RumaFormattedBody {
    fn from(f: FormattedBody) -> Self {
        Self {
            format: match f.format {
                MessageFormat::Html => matrix_sdk::ruma::events::room::message::MessageFormat::Html,
                MessageFormat::Unknown { format } => format.into(),
            },
            body: f.body,
        }
    }
}

impl From<&RumaFormattedBody> for FormattedBody {
    fn from(f: &RumaFormattedBody) -> Self {
        Self {
            format: match &f.format {
                matrix_sdk::ruma::events::room::message::MessageFormat::Html => MessageFormat::Html,
                _ => MessageFormat::Unknown { format: f.format.to_string() },
            },
            body: f.body.clone(),
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MessageFormat {
    Html,
    Unknown { format: String },
}

impl From<&matrix_sdk::ruma::events::room::ImageInfo> for ImageInfo {
    fn from(info: &matrix_sdk::ruma::events::room::ImageInfo) -> Self {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Self {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info.thumbnail_source.clone().map(Arc::new),
            blurhash: info.blurhash.clone(),
        }
    }
}

impl From<&RumaAudioInfo> for AudioInfo {
    fn from(info: &RumaAudioInfo) -> Self {
        Self {
            duration: info.duration,
            size: info.size.map(Into::into),
            mimetype: info.mimetype.clone(),
        }
    }
}

impl From<&RumaVideoInfo> for VideoInfo {
    fn from(info: &RumaVideoInfo) -> Self {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Self {
            duration: info.duration,
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info.thumbnail_source.clone().map(Arc::new),
            blurhash: info.blurhash.clone(),
        }
    }
}

impl From<&RumaFileInfo> for FileInfo {
    fn from(info: &RumaFileInfo) -> Self {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Self {
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info.thumbnail_source.clone().map(Arc::new),
        }
    }
}

pub(crate) fn u64_to_uint(u: u64) -> UInt {
    UInt::new(u).unwrap_or_else(|| {
        warn!("u64 -> UInt conversion overflowed, falling back to UInt::MAX");
        UInt::MAX
    })
}

#[derive(uniffi::Record)]
pub struct InReplyToDetails {
    event_id: String,
    event: RepliedToEventDetails,
}

impl From<&matrix_sdk_ui::timeline::InReplyToDetails> for InReplyToDetails {
    fn from(inner: &matrix_sdk_ui::timeline::InReplyToDetails) -> Self {
        let event_id = inner.event_id.to_string();
        let event = match &inner.event {
            TimelineDetails::Unavailable => RepliedToEventDetails::Unavailable,
            TimelineDetails::Pending => RepliedToEventDetails::Pending,
            TimelineDetails::Ready(event) => RepliedToEventDetails::Ready {
                content: Arc::new(TimelineItemContent(event.content().to_owned())),
                sender: event.sender().to_string(),
                sender_profile: event.sender_profile().into(),
            },
            TimelineDetails::Error(err) => {
                RepliedToEventDetails::Error { message: err.to_string() }
            }
        };

        Self { event_id, event }
    }
}

#[derive(uniffi::Enum)]
pub enum RepliedToEventDetails {
    Unavailable,
    Pending,
    Ready { content: Arc<TimelineItemContent>, sender: String, sender_profile: ProfileDetails },
    Error { message: String },
}

#[derive(Clone, uniffi::Enum)]
pub enum EncryptedMessage {
    OlmV1Curve25519AesSha2 {
        /// The Curve25519 key of the sender.
        sender_key: String,
    },
    // Other fields not included because UniFFI doesn't have the concept of
    // deprecated fields right now.
    MegolmV1AesSha2 {
        /// The ID of the session used to encrypt the message.
        session_id: String,
    },
    Unknown,
}

impl EncryptedMessage {
    fn new(msg: &matrix_sdk_ui::timeline::EncryptedMessage) -> Self {
        use matrix_sdk_ui::timeline::EncryptedMessage as Message;

        match msg {
            Message::OlmV1Curve25519AesSha2 { sender_key } => {
                let sender_key = sender_key.clone();
                Self::OlmV1Curve25519AesSha2 { sender_key }
            }
            Message::MegolmV1AesSha2 { session_id, .. } => {
                let session_id = session_id.clone();
                Self::MegolmV1AesSha2 { session_id }
            }
            Message::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct Reaction {
    pub key: String,
    pub count: u64,
    pub senders: Vec<ReactionSenderData>,
}

#[derive(Clone, uniffi::Record)]
pub struct ReactionSenderData {
    pub sender_id: String,
    pub timestamp: u64,
}

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(uniffi::Enum)]
pub enum VirtualTimelineItem {
    /// A divider between messages of two days.
    DayDivider {
        /// A timestamp in milliseconds since Unix Epoch on that day in local
        /// time.
        ts: u64,
    },

    /// The user's own read marker.
    ReadMarker,
}

#[extension_trait]
pub impl MediaSourceExt for MediaSource {
    fn from_json(json: String) -> Result<MediaSource, ClientError> {
        let res = serde_json::from_str(&json)?;
        Ok(res)
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Media source should always be serializable ")
    }

    fn url(&self) -> String {
        match self {
            MediaSource::Plain(url) => url.to_string(),
            MediaSource::Encrypted(file) => file.url.to_string(),
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MembershipChange {
    None,
    Error,
    Joined,
    Left,
    Banned,
    Unbanned,
    Kicked,
    Invited,
    KickedAndBanned,
    InvitationAccepted,
    InvitationRejected,
    InvitationRevoked,
    Knocked,
    KnockAccepted,
    KnockRetracted,
    KnockDenied,
    NotImplemented,
}

impl From<matrix_sdk_ui::timeline::MembershipChange> for MembershipChange {
    fn from(membership_change: matrix_sdk_ui::timeline::MembershipChange) -> Self {
        use matrix_sdk_ui::timeline::MembershipChange as Change;
        match membership_change {
            Change::None => Self::None,
            Change::Error => Self::Error,
            Change::Joined => Self::Joined,
            Change::Left => Self::Left,
            Change::Banned => Self::Banned,
            Change::Unbanned => Self::Unbanned,
            Change::Kicked => Self::Kicked,
            Change::Invited => Self::Invited,
            Change::KickedAndBanned => Self::KickedAndBanned,
            Change::InvitationAccepted => Self::InvitationAccepted,
            Change::InvitationRejected => Self::InvitationRejected,
            Change::InvitationRevoked => Self::InvitationRevoked,
            Change::Knocked => Self::Knocked,
            Change::KnockAccepted => Self::KnockAccepted,
            Change::KnockRetracted => Self::KnockRetracted,
            Change::KnockDenied => Self::KnockDenied,
            Change::NotImplemented => Self::NotImplemented,
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum OtherState {
    PolicyRuleRoom,
    PolicyRuleServer,
    PolicyRuleUser,
    RoomAliases,
    RoomAvatar { url: Option<String> },
    RoomCanonicalAlias,
    RoomCreate,
    RoomEncryption,
    RoomGuestAccess,
    RoomHistoryVisibility,
    RoomJoinRules,
    RoomName { name: Option<String> },
    RoomPinnedEvents,
    RoomPowerLevels,
    RoomServerAcl,
    RoomThirdPartyInvite { display_name: Option<String> },
    RoomTombstone,
    RoomTopic { topic: Option<String> },
    SpaceChild,
    SpaceParent,
    Custom { event_type: String },
}

impl From<&matrix_sdk_ui::timeline::AnyOtherFullStateEventContent> for OtherState {
    fn from(content: &matrix_sdk_ui::timeline::AnyOtherFullStateEventContent) -> Self {
        use matrix_sdk::ruma::events::FullStateEventContent as FullContent;
        use matrix_sdk_ui::timeline::AnyOtherFullStateEventContent as Content;

        match content {
            Content::PolicyRuleRoom(_) => Self::PolicyRuleRoom,
            Content::PolicyRuleServer(_) => Self::PolicyRuleServer,
            Content::PolicyRuleUser(_) => Self::PolicyRuleUser,
            Content::RoomAliases(_) => Self::RoomAliases,
            Content::RoomAvatar(c) => {
                let url = match c {
                    FullContent::Original { content, .. } => {
                        content.url.as_ref().map(ToString::to_string)
                    }
                    FullContent::Redacted(_) => None,
                };
                Self::RoomAvatar { url }
            }
            Content::RoomCanonicalAlias(_) => Self::RoomCanonicalAlias,
            Content::RoomCreate(_) => Self::RoomCreate,
            Content::RoomEncryption(_) => Self::RoomEncryption,
            Content::RoomGuestAccess(_) => Self::RoomGuestAccess,
            Content::RoomHistoryVisibility(_) => Self::RoomHistoryVisibility,
            Content::RoomJoinRules(_) => Self::RoomJoinRules,
            Content::RoomName(c) => {
                let name = match c {
                    FullContent::Original { content, .. } => content.name.clone(),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomName { name }
            }
            Content::RoomPinnedEvents(_) => Self::RoomPinnedEvents,
            Content::RoomPowerLevels(_) => Self::RoomPowerLevels,
            Content::RoomServerAcl(_) => Self::RoomServerAcl,
            Content::RoomThirdPartyInvite(c) => {
                let display_name = match c {
                    FullContent::Original { content, .. } => Some(content.display_name.clone()),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomThirdPartyInvite { display_name }
            }
            Content::RoomTombstone(_) => Self::RoomTombstone,
            Content::RoomTopic(c) => {
                let topic = match c {
                    FullContent::Original { content, .. } => Some(content.topic.clone()),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomTopic { topic }
            }
            Content::SpaceChild(_) => Self::SpaceChild,
            Content::SpaceParent(_) => Self::SpaceParent,
            Content::_Custom { event_type, .. } => Self::Custom { event_type: event_type.clone() },
        }
    }
}

#[derive(uniffi::Enum)]
pub enum PollKind {
    Disclosed,
    Undisclosed,
}

impl From<PollKind> for RumaPollKind {
    fn from(value: PollKind) -> Self {
        match value {
            PollKind::Disclosed => Self::Disclosed,
            PollKind::Undisclosed => Self::Undisclosed,
        }
    }
}

impl From<RumaPollKind> for PollKind {
    fn from(value: RumaPollKind) -> Self {
        match value {
            RumaPollKind::Disclosed => Self::Disclosed,
            RumaPollKind::Undisclosed => Self::Undisclosed,
            _ => {
                info!("Unknown poll kind, defaulting to undisclosed");
                Self::Undisclosed
            }
        }
    }
}

#[derive(uniffi::Record)]
pub struct PollAnswer {
    pub id: String,
    pub text: String,
}

impl From<PollResult> for TimelineItemContentKind {
    fn from(value: PollResult) -> Self {
        TimelineItemContentKind::Poll {
            question: value.question,
            kind: PollKind::from(value.kind),
            max_selections: value.max_selections,
            answers: value
                .answers
                .into_iter()
                .map(|i| PollAnswer { id: i.id, text: i.text })
                .collect(),
            votes: value.votes,
            end_time: value.end_time,
        }
    }
}
