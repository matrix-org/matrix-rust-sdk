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

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use extension_trait::extension_trait;
use matrix_sdk::attachment::{
    BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
};
use ruma::{
    assign,
    events::{
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
    serde::JsonObject,
    OwnedUserId, UInt, UserId,
};
use tracing::info;

use crate::{
    error::{ClientError, MediaInfoError},
    helpers::unwrap_or_clone_arc,
    utils::u64_to_uint,
};

#[uniffi::export]
pub fn media_source_from_url(url: String) -> Arc<MediaSource> {
    Arc::new(MediaSource::Plain(url.into()))
}

#[uniffi::export]
pub fn message_event_content_new(
    msgtype: MessageType,
) -> Result<Arc<RoomMessageEventContentWithoutRelation>, ClientError> {
    Ok(Arc::new(RoomMessageEventContentWithoutRelation::new(msgtype.try_into()?)))
}

#[uniffi::export]
pub fn message_event_content_from_markdown(
    md: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::text_markdown(md)))
}

#[uniffi::export]
pub fn message_event_content_from_markdown_as_emote(
    md: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::emote_markdown(md)))
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

#[uniffi::export]
pub fn message_event_content_from_html_as_emote(
    body: String,
    html_body: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::emote_html(
        body, html_body,
    )))
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

#[extension_trait]
pub impl RoomMessageEventContentWithoutRelationExt for RoomMessageEventContentWithoutRelation {
    fn with_mentions(self: Arc<Self>, mentions: Mentions) -> Arc<Self> {
        let mut content = unwrap_or_clone_arc(self);
        content.mentions = Some(mentions.into());
        Arc::new(content)
    }
}

pub struct Mentions {
    pub user_ids: Vec<String>,
    pub room: bool,
}

impl From<Mentions> for ruma::events::Mentions {
    fn from(value: Mentions) -> Self {
        let mut user_ids = BTreeSet::<OwnedUserId>::new();
        for user_id in value.user_ids {
            if let Ok(user_id) = UserId::parse(user_id) {
                user_ids.insert(user_id);
            }
        }
        let mut result = Self::default();
        result.user_ids = user_ids;
        result.room = value.room;
        result
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
    Other { msgtype: String, body: String },
}

impl TryFrom<MessageType> for RumaMessageType {
    type Error = serde_json::Error;

    fn try_from(value: MessageType) -> Result<Self, Self::Error> {
        Ok(match value {
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
            MessageType::Other { msgtype, body } => {
                Self::new(&msgtype, body, JsonObject::default())?
            }
        })
    }
}

impl From<RumaMessageType> for MessageType {
    fn from(value: RumaMessageType) -> Self {
        match value {
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
            _ => MessageType::Other {
                msgtype: value.msgtype().to_owned(),
                body: value.body().to_owned(),
            },
        }
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

#[derive(Clone, uniffi::Enum)]
pub enum AssetType {
    Sender,
    Pin,
}

impl From<AssetType> for RumaAssetType {
    fn from(value: AssetType) -> Self {
        match value {
            AssetType::Sender => Self::Self_,
            AssetType::Pin => Self::Pin,
        }
    }
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
