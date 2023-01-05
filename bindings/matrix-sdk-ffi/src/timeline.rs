use std::sync::Arc;

use extension_trait::extension_trait;
use futures_signals::signal_vec::VecDiff;
pub use matrix_sdk::ruma::events::room::{message::RoomMessageEventContent, MediaSource};

#[uniffi::export]
pub fn media_source_from_url(url: String) -> Arc<MediaSource> {
    Arc::new(MediaSource::Plain(url.into()))
}

#[uniffi::export]
pub fn message_event_content_from_markdown(md: String) -> Arc<RoomMessageEventContent> {
    Arc::new(RoomMessageEventContent::text_markdown(md))
}

pub trait TimelineListener: Sync + Send {
    fn on_update(&self, diff: Arc<TimelineDiff>);
}

#[repr(transparent)]
#[derive(Clone)]
pub struct TimelineDiff(VecDiff<Arc<TimelineItem>>);

impl TimelineDiff {
    pub(crate) fn new(inner: VecDiff<Arc<matrix_sdk::room::timeline::TimelineItem>>) -> Self {
        TimelineDiff(match inner {
            // Note: It's _probably_ valid to only transmute here too but not
            //       as clear, and less important because this only happens
            //       once when constructing the timeline.
            VecDiff::Replace { values } => VecDiff::Replace {
                values: values.into_iter().map(TimelineItem::from_arc).collect(),
            },
            VecDiff::InsertAt { index, value } => {
                VecDiff::InsertAt { index, value: TimelineItem::from_arc(value) }
            }
            VecDiff::UpdateAt { index, value } => {
                VecDiff::UpdateAt { index, value: TimelineItem::from_arc(value) }
            }
            VecDiff::RemoveAt { index } => VecDiff::RemoveAt { index },
            VecDiff::Move { old_index, new_index } => VecDiff::Move { old_index, new_index },
            VecDiff::Push { value } => VecDiff::Push { value: TimelineItem::from_arc(value) },
            VecDiff::Pop {} => VecDiff::Pop {},
            VecDiff::Clear {} => VecDiff::Clear {},
        })
    }
}

#[uniffi::export]
impl TimelineDiff {
    pub fn change(&self) -> TimelineChange {
        match &self.0 {
            VecDiff::Replace { .. } => TimelineChange::Replace,
            VecDiff::InsertAt { .. } => TimelineChange::InsertAt,
            VecDiff::UpdateAt { .. } => TimelineChange::UpdateAt,
            VecDiff::RemoveAt { .. } => TimelineChange::RemoveAt,
            VecDiff::Move { .. } => TimelineChange::Move,
            VecDiff::Push { .. } => TimelineChange::Push,
            VecDiff::Pop {} => TimelineChange::Pop,
            VecDiff::Clear {} => TimelineChange::Clear,
        }
    }

    pub fn replace(self: Arc<Self>) -> Option<Vec<Arc<TimelineItem>>> {
        unwrap_or_clone_arc_into_variant!(self, .0, VecDiff::Replace { values } => values)
    }

    pub fn insert_at(self: Arc<Self>) -> Option<InsertAtData> {
        unwrap_or_clone_arc_into_variant!(self, .0, VecDiff::InsertAt { index, value } => {
            InsertAtData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn update_at(self: Arc<Self>) -> Option<UpdateAtData> {
        unwrap_or_clone_arc_into_variant!(self, .0, VecDiff::UpdateAt { index, value } => {
            UpdateAtData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn remove_at(&self) -> Option<u32> {
        match &self.0 {
            VecDiff::RemoveAt { index } => Some((*index).try_into().unwrap()),
            _ => None,
        }
    }

    pub fn push(self: Arc<Self>) -> Option<Arc<TimelineItem>> {
        unwrap_or_clone_arc_into_variant!(self, .0, VecDiff::Push { value } => value)
    }
}

// UniFFI currently chokes on the r#
impl TimelineDiff {
    pub fn r#move(&self) -> Option<MoveData> {
        match &self.0 {
            VecDiff::Move { old_index, new_index } => Some(MoveData {
                old_index: (*old_index).try_into().unwrap(),
                new_index: (*new_index).try_into().unwrap(),
            }),
            _ => None,
        }
    }
}

#[derive(uniffi::Record)]
pub struct InsertAtData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

#[derive(uniffi::Record)]
pub struct UpdateAtData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

pub struct MoveData {
    pub old_index: u32,
    pub new_index: u32,
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum TimelineChange {
    Replace,
    InsertAt,
    UpdateAt,
    RemoveAt,
    Move,
    Push,
    Pop,
    Clear,
}

#[repr(transparent)]
#[derive(Clone, uniffi::Object)]
pub struct TimelineItem(matrix_sdk::room::timeline::TimelineItem);

impl TimelineItem {
    fn from_arc(arc: Arc<matrix_sdk::room::timeline::TimelineItem>) -> Arc<Self> {
        // SAFETY: This is valid because Self is a repr(transparent) wrapper
        //         around the other Timeline type.
        unsafe { Arc::from_raw(Arc::into_raw(arc) as _) }
    }
}

#[uniffi::export]
impl TimelineItem {
    pub fn as_event(self: Arc<Self>) -> Option<Arc<EventTimelineItem>> {
        use matrix_sdk::room::timeline::TimelineItem as Item;
        unwrap_or_clone_arc_into_variant!(self, .0, Item::Event(evt) => {
            Arc::new(EventTimelineItem(evt))
        })
    }

    pub fn as_virtual(self: Arc<Self>) -> Option<VirtualTimelineItem> {
        use matrix_sdk::room::timeline::{TimelineItem as Item, VirtualTimelineItem as VItem};
        match &self.0 {
            Item::Virtual(VItem::DayDivider { year, month, day }) => {
                Some(VirtualTimelineItem::DayDivider { year: *year, month: *month, day: *day })
            }
            Item::Virtual(VItem::ReadMarker) => Some(VirtualTimelineItem::ReadMarker),
            Item::Event(_) => None,
        }
    }

    pub fn fmt_debug(&self) -> String {
        format!("{:#?}", self.0)
    }
}

#[derive(uniffi::Object)]
pub struct EventTimelineItem(pub(crate) matrix_sdk::room::timeline::EventTimelineItem);

#[uniffi::export]
impl EventTimelineItem {
    pub fn key(&self) -> TimelineKey {
        self.0.key().into()
    }

    pub fn event_id(&self) -> Option<String> {
        self.0.event_id().map(ToString::to_string)
    }

    pub fn sender(&self) -> String {
        self.0.sender().to_string()
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
            .map(|(k, v)| Reaction { key: k.to_owned(), count: v.count.into() })
            .collect()
    }

    pub fn raw(&self) -> Option<String> {
        self.0.raw().map(|r| r.json().get().to_owned())
    }

    pub fn fmt_debug(&self) -> String {
        format!("{:#?}", self.0)
    }
}

#[derive(Clone, uniffi::Object)]
pub struct TimelineItemContent(matrix_sdk::room::timeline::TimelineItemContent);

#[uniffi::export]
impl TimelineItemContent {
    pub fn kind(&self) -> TimelineItemContentKind {
        use matrix_sdk::room::timeline::TimelineItemContent as Content;

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
            Content::UnableToDecrypt(msg) => {
                TimelineItemContentKind::UnableToDecrypt { msg: EncryptedMessage::new(msg) }
            }
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
        use matrix_sdk::room::timeline::TimelineItemContent as Content;
        unwrap_or_clone_arc_into_variant!(self, .0, Content::Message(msg) => Arc::new(Message(msg)))
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineItemContentKind {
    Message,
    RedactedMessage,
    Sticker { body: String, info: ImageInfo, url: String },
    UnableToDecrypt { msg: EncryptedMessage },
    FailedToParseMessageLike { event_type: String, error: String },
    FailedToParseState { event_type: String, state_key: String, error: String },
}

#[derive(Clone, uniffi::Object)]
pub struct Message(matrix_sdk::room::timeline::Message);

#[uniffi::export]
impl Message {
    pub fn msgtype(&self) -> Option<MessageType> {
        use matrix_sdk::ruma::events::room::message::MessageType as MTy;
        match self.0.msgtype() {
            MTy::Emote(c) => Some(MessageType::Emote {
                content: EmoteMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            }),
            MTy::Image(c) => Some(MessageType::Image {
                content: ImageMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            }),
            MTy::Video(c) => Some(MessageType::Video {
                content: VideoMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            }),
            MTy::File(c) => Some(MessageType::File {
                content: FileMessageContent {
                    body: c.body.clone(),
                    source: Arc::new(c.source.clone()),
                    info: c.info.as_deref().map(Into::into),
                },
            }),
            MTy::Notice(c) => Some(MessageType::Notice {
                content: NoticeMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            }),
            MTy::Text(c) => Some(MessageType::Text {
                content: TextMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            }),
            _ => None,
        }
    }

    pub fn body(&self) -> String {
        self.0.msgtype().body().to_owned()
    }

    // This event ID string will be replaced by something more useful later.
    pub fn in_reply_to(&self) -> Option<String> {
        self.0.in_reply_to().map(ToString::to_string)
    }

    pub fn is_edited(&self) -> bool {
        self.0.is_edited()
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MessageType {
    Emote { content: EmoteMessageContent },
    Image { content: ImageMessageContent },
    Video { content: VideoMessageContent },
    File { content: FileMessageContent },
    Notice { content: NoticeMessageContent },
    Text { content: TextMessageContent },
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
pub struct VideoMessageContent {
    pub body: String,
    pub source: Arc<MediaSource>,
    pub info: Option<VideoInfo>,
}

#[derive(Clone, uniffi::Record)]
pub struct FileMessageContent {
    pub body: String,
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

#[derive(Clone, uniffi::Record)]
pub struct VideoInfo {
    pub duration: Option<u64>,
    pub height: Option<u64>,
    pub width: Option<u64>,
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub thumbnail_info: Option<ThumbnailInfo>,
    pub thumbnail_source: Option<Arc<MediaSource>>,
    pub blurhash: Option<String>,
}

#[derive(Clone, uniffi::Record)]
pub struct FileInfo {
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub thumbnail_info: Option<ThumbnailInfo>,
    pub thumbnail_source: Option<Arc<MediaSource>>,
}

#[derive(Clone, uniffi::Record)]
pub struct ThumbnailInfo {
    pub height: Option<u64>,
    pub width: Option<u64>,
    pub mimetype: Option<String>,
    pub size: Option<u64>,
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
pub struct FormattedBody {
    pub format: MessageFormat,
    pub body: String,
}

impl From<&matrix_sdk::ruma::events::room::message::FormattedBody> for FormattedBody {
    fn from(f: &matrix_sdk::ruma::events::room::message::FormattedBody) -> Self {
        Self {
            format: match &f.format {
                matrix_sdk::ruma::events::room::message::MessageFormat::Html => MessageFormat::Html,
                _ => MessageFormat::Unknown,
            },
            body: f.body.clone(),
        }
    }
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum MessageFormat {
    Html,
    Unknown,
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

impl From<&matrix_sdk::ruma::events::room::message::VideoInfo> for VideoInfo {
    fn from(info: &matrix_sdk::ruma::events::room::message::VideoInfo) -> Self {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Self {
            duration: info.duration.map(|d| d.as_secs()),
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

impl From<&matrix_sdk::ruma::events::room::message::FileInfo> for FileInfo {
    fn from(info: &matrix_sdk::ruma::events::room::message::FileInfo) -> Self {
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
    fn new(msg: &matrix_sdk::room::timeline::EncryptedMessage) -> Self {
        use matrix_sdk::room::timeline::EncryptedMessage as Message;

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
    // TODO: Also expose senders
}

#[derive(Clone)]
pub struct ReactionDetails {
    pub id: TimelineKey,
    pub sender: String,
}

#[derive(Clone, uniffi::Enum)]
pub enum TimelineKey {
    TransactionId { txn_id: String },
    EventId { event_id: String },
}

impl From<&matrix_sdk::room::timeline::TimelineKey> for TimelineKey {
    fn from(timeline_key: &matrix_sdk::room::timeline::TimelineKey) -> Self {
        use matrix_sdk::room::timeline::TimelineKey::*;

        match timeline_key {
            TransactionId(txn_id) => TimelineKey::TransactionId { txn_id: txn_id.to_string() },
            EventId(event_id) => TimelineKey::EventId { event_id: event_id.to_string() },
        }
    }
}

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(uniffi::Enum)]
pub enum VirtualTimelineItem {
    /// A divider between messages of two days.
    DayDivider {
        /// The year.
        year: i32,
        /// The month of the year.
        ///
        /// A value between 1 and 12.
        month: u32,
        /// The day of the month.
        ///
        /// A value between 1 and 31.
        day: u32,
    },
    /// The user's own read marker.
    ReadMarker,
}

#[extension_trait]
pub impl MediaSourceExt for MediaSource {
    fn url(&self) -> String {
        match self {
            MediaSource::Plain(url) => url.to_string(),
            MediaSource::Encrypted(file) => file.url.to_string(),
        }
    }
}
