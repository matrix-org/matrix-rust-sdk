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

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use extension_trait::extension_trait;
use matrix_sdk::attachment::{BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo};
use ruma::{
    assign,
    events::{
        direct::DirectEventContent,
        fully_read::FullyReadEventContent,
        identity_server::IdentityServerEventContent,
        ignored_user_list::{IgnoredUser as RumaIgnoredUser, IgnoredUserListEventContent},
        location::AssetType as RumaAssetType,
        marked_unread::{MarkedUnreadEventContent, UnstableMarkedUnreadEventContent},
        media_preview_config::{
            InviteAvatars as RumaInviteAvatars, MediaPreviewConfigEventContent,
            MediaPreviews as RumaMediaPreviews,
        },
        poll::start::PollKind as RumaPollKind,
        push_rules::PushRulesEventContent,
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
                TextMessageEventContent as RumaTextMessageEventContent, UnstableAmplitude,
                UnstableAudioDetailsContentBlock as RumaUnstableAudioDetailsContentBlock,
                UnstableVoiceContentBlock as RumaUnstableVoiceContentBlock,
                VideoInfo as RumaVideoInfo,
                VideoMessageEventContent as RumaVideoMessageEventContent,
            },
            ImageInfo as RumaImageInfo, MediaSource as RumaMediaSource,
            ThumbnailInfo as RumaThumbnailInfo,
        },
        rtc::notification::NotificationType as RumaNotificationType,
        secret_storage::{
            default_key::SecretStorageDefaultKeyEventContent,
            key::{
                PassPhrase as RumaPassPhrase,
                SecretStorageEncryptionAlgorithm as RumaSecretStorageEncryptionAlgorithm,
                SecretStorageKeyEventContent,
                SecretStorageV1AesHmacSha2Properties as RumaSecretStorageV1AesHmacSha2Properties,
            },
        },
        tag::{
            TagEventContent, TagInfo as RumaTagInfo, TagName as RumaTagName,
            UserTagName as RumaUserTagName,
        },
        GlobalAccountDataEvent as RumaGlobalAccountDataEvent,
        GlobalAccountDataEventType as RumaGlobalAccountDataEventType,
        RoomAccountDataEvent as RumaRoomAccountDataEvent,
        RoomAccountDataEventType as RumaRoomAccountDataEventType,
    },
    matrix_uri::MatrixId as RumaMatrixId,
    push::{
        ConditionalPushRule as RumaConditionalPushRule, PatternedPushRule as RumaPatternedPushRule,
        Ruleset as RumaRuleset, SimplePushRule as RumaSimplePushRule,
    },
    serde::JsonObject,
    KeyDerivationAlgorithm as RumaKeyDerivationAlgorithm, MatrixToUri, MatrixUri as RumaMatrixUri,
    OwnedRoomId, OwnedUserId, UInt, UserId,
};
use tracing::info;

use crate::{
    error::{ClientError, MediaInfoError},
    helpers::unwrap_or_clone_arc,
    notification_settings::{Action, PushCondition},
    timeline::MessageContent,
    utils::u64_to_uint,
};

#[derive(uniffi::Enum)]
pub enum AuthData {
    /// Password-based authentication (`m.login.password`).
    Password { password_details: AuthDataPasswordDetails },
}

#[derive(uniffi::Record)]
pub struct AuthDataPasswordDetails {
    /// One of the user's identifiers.
    identifier: String,

    /// The plaintext password.
    password: String,
}

impl From<AuthData> for ruma::api::client::uiaa::AuthData {
    fn from(value: AuthData) -> ruma::api::client::uiaa::AuthData {
        match value {
            AuthData::Password { password_details } => {
                let user_id = ruma::UserId::parse(password_details.identifier).unwrap();

                ruma::api::client::uiaa::AuthData::Password(ruma::api::client::uiaa::Password::new(
                    user_id.into(),
                    password_details.password,
                ))
            }
        }
    }
}

/// Parse a matrix entity from a given URI, be it either
/// a `matrix.to` link or a `matrix:` URI
#[matrix_sdk_ffi_macros::export]
pub fn parse_matrix_entity_from(uri: String) -> Option<MatrixEntity> {
    if let Ok(matrix_uri) = RumaMatrixUri::parse(&uri) {
        return Some(MatrixEntity {
            id: matrix_uri.id().into(),
            via: matrix_uri.via().iter().map(|via| via.to_string()).collect(),
        });
    }

    if let Ok(matrix_to_uri) = MatrixToUri::parse(&uri) {
        return Some(MatrixEntity {
            id: matrix_to_uri.id().into(),
            via: matrix_to_uri.via().iter().map(|via| via.to_string()).collect(),
        });
    }

    None
}

/// A Matrix entity that can be a room, room alias, user, or event, and a list
/// of via servers.
#[derive(uniffi::Record)]
pub struct MatrixEntity {
    id: MatrixId,
    via: Vec<String>,
}

/// A Matrix ID that can be a room, room alias, user, or event.
#[derive(Clone, uniffi::Enum)]
pub enum MatrixId {
    Room { id: String },
    RoomAlias { alias: String },
    User { id: String },
    EventOnRoomId { room_id: String, event_id: String },
    EventOnRoomAlias { alias: String, event_id: String },
}

impl From<&RumaMatrixId> for MatrixId {
    fn from(value: &RumaMatrixId) -> Self {
        match value {
            RumaMatrixId::User(id) => MatrixId::User { id: id.to_string() },
            RumaMatrixId::Room(id) => MatrixId::Room { id: id.to_string() },
            RumaMatrixId::RoomAlias(id) => MatrixId::RoomAlias { alias: id.to_string() },

            RumaMatrixId::Event(room_id_or_alias, event_id) => {
                if room_id_or_alias.is_room_id() {
                    MatrixId::EventOnRoomId {
                        room_id: room_id_or_alias.to_string(),
                        event_id: event_id.to_string(),
                    }
                } else if room_id_or_alias.is_room_alias_id() {
                    MatrixId::EventOnRoomAlias {
                        alias: room_id_or_alias.to_string(),
                        event_id: event_id.to_string(),
                    }
                } else {
                    panic!("Unexpected MatrixId type: {room_id_or_alias:?}")
                }
            }
            _ => panic!("Unexpected MatrixId type: {value:?}"),
        }
    }
}

#[matrix_sdk_ffi_macros::export]
pub fn message_event_content_new(
    msgtype: MessageType,
) -> Result<Arc<RoomMessageEventContentWithoutRelation>, ClientError> {
    Ok(Arc::new(RoomMessageEventContentWithoutRelation::new(msgtype.try_into()?)))
}

#[matrix_sdk_ffi_macros::export]
pub fn message_event_content_from_markdown(
    md: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::text_markdown(md)))
}

#[matrix_sdk_ffi_macros::export]
pub fn message_event_content_from_markdown_as_emote(
    md: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::emote_markdown(md)))
}

#[matrix_sdk_ffi_macros::export]
pub fn message_event_content_from_html(
    body: String,
    html_body: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::text_html(
        body, html_body,
    )))
}

#[matrix_sdk_ffi_macros::export]
pub fn message_event_content_from_html_as_emote(
    body: String,
    html_body: String,
) -> Arc<RoomMessageEventContentWithoutRelation> {
    Arc::new(RoomMessageEventContentWithoutRelation::new(RumaMessageType::emote_html(
        body, html_body,
    )))
}

#[derive(Clone, uniffi::Object)]
pub struct MediaSource {
    pub(crate) media_source: RumaMediaSource,
}

#[matrix_sdk_ffi_macros::export]
impl MediaSource {
    #[uniffi::constructor]
    pub fn from_url(url: String) -> Result<Arc<MediaSource>, ClientError> {
        let media_source = RumaMediaSource::Plain(url.into());
        media_source.verify()?;

        Ok(Arc::new(MediaSource { media_source }))
    }

    pub fn url(&self) -> String {
        self.media_source.url()
    }

    // Used on Element X Android
    #[uniffi::constructor]
    pub fn from_json(json: String) -> Result<Arc<Self>, ClientError> {
        let media_source: RumaMediaSource = serde_json::from_str(&json)?;
        media_source.verify()?;

        Ok(Arc::new(MediaSource { media_source }))
    }

    // Used on Element X Android
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.media_source)
            .expect("Media source should always be serializable ")
    }
}

impl TryFrom<RumaMediaSource> for MediaSource {
    type Error = ClientError;

    fn try_from(value: RumaMediaSource) -> Result<Self, Self::Error> {
        value.verify()?;
        Ok(Self { media_source: value })
    }
}

impl TryFrom<&RumaMediaSource> for MediaSource {
    type Error = ClientError;

    fn try_from(value: &RumaMediaSource) -> Result<Self, Self::Error> {
        value.verify()?;
        Ok(Self { media_source: value.clone() })
    }
}

impl From<MediaSource> for RumaMediaSource {
    fn from(value: MediaSource) -> Self {
        value.media_source
    }
}

#[extension_trait]
pub(crate) impl MediaSourceExt for RumaMediaSource {
    fn verify(&self) -> Result<(), ClientError> {
        match self {
            RumaMediaSource::Plain(url) => {
                url.validate().map_err(ClientError::from_err)?;
            }
            RumaMediaSource::Encrypted(file) => {
                file.url.validate().map_err(ClientError::from_err)?;
            }
        }

        Ok(())
    }

    fn url(&self) -> String {
        match self {
            RumaMediaSource::Plain(url) => url.to_string(),
            RumaMediaSource::Encrypted(file) => file.url.to_string(),
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

#[derive(Clone)]
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
    Emote {
        content: EmoteMessageContent,
    },
    Image {
        content: ImageMessageContent,
    },
    Audio {
        content: AudioMessageContent,
    },
    Video {
        content: VideoMessageContent,
    },
    File {
        content: FileMessageContent,
    },
    #[cfg(feature = "unstable-msc4274")]
    Gallery {
        content: GalleryMessageContent,
    },
    Notice {
        content: NoticeMessageContent,
    },
    Text {
        content: TextMessageContent,
    },
    Location {
        content: LocationContent,
    },
    Other {
        msgtype: String,
        body: String,
    },
}

/// From MSC2530: https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/2530-body-as-caption.md
/// If the filename field is present in a media message, clients should treat
/// body as a caption instead of a file name. Otherwise, the body is the
/// file name.
///
/// So:
/// - if a media has a filename and a caption, the body is the caption, filename
///   is its own field.
/// - if a media only has a filename, then body is the filename.
fn get_body_and_filename(filename: String, caption: Option<String>) -> (String, Option<String>) {
    if let Some(caption) = caption {
        (caption, Some(filename))
    } else {
        (filename, None)
    }
}

impl TryFrom<MessageType> for RumaMessageType {
    type Error = ClientError;

    fn try_from(value: MessageType) -> Result<Self, Self::Error> {
        Ok(match value {
            MessageType::Emote { content } => {
                Self::Emote(assign!(RumaEmoteMessageEventContent::plain(content.body), {
                    formatted: content.formatted.map(Into::into),
                }))
            }
            MessageType::Image { content } => Self::Image(content.into()),
            MessageType::Audio { content } => Self::Audio(content.into()),
            MessageType::Video { content } => Self::Video(content.into()),
            MessageType::File { content } => Self::File(content.into()),
            #[cfg(feature = "unstable-msc4274")]
            MessageType::Gallery { content } => Self::Gallery(content.try_into()?),
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

impl TryFrom<RumaMessageType> for MessageType {
    type Error = ClientError;

    fn try_from(value: RumaMessageType) -> Result<Self, Self::Error> {
        Ok(match value {
            RumaMessageType::Emote(c) => MessageType::Emote {
                content: EmoteMessageContent {
                    body: c.body.clone(),
                    formatted: c.formatted.as_ref().map(Into::into),
                },
            },
            RumaMessageType::Image(c) => MessageType::Image { content: c.try_into()? },
            RumaMessageType::Audio(c) => MessageType::Audio { content: c.try_into()? },
            RumaMessageType::Video(c) => MessageType::Video { content: c.try_into()? },
            RumaMessageType::File(c) => MessageType::File { content: c.try_into()? },
            #[cfg(feature = "unstable-msc4274")]
            RumaMessageType::Gallery(c) => MessageType::Gallery { content: c.try_into()? },
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
        })
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum RtcNotificationType {
    Ring,
    Notification,
}

impl From<RumaNotificationType> for RtcNotificationType {
    fn from(val: RumaNotificationType) -> Self {
        match val {
            RumaNotificationType::Ring => Self::Ring,
            _ => Self::Notification,
        }
    }
}

impl From<RtcNotificationType> for RumaNotificationType {
    fn from(value: RtcNotificationType) -> Self {
        match value {
            RtcNotificationType::Ring => RumaNotificationType::Ring,
            RtcNotificationType::Notification => RumaNotificationType::Notification,
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
    /// The computed filename, for use in a client.
    pub filename: String,
    pub caption: Option<String>,
    pub formatted_caption: Option<FormattedBody>,
    pub source: Arc<MediaSource>,
    pub info: Option<ImageInfo>,
}

impl From<ImageMessageContent> for RumaImageMessageEventContent {
    fn from(value: ImageMessageContent) -> Self {
        let (body, filename) = get_body_and_filename(value.filename, value.caption);
        let mut event_content = Self::new(body, (*value.source).clone().into())
            .info(value.info.map(Into::into).map(Box::new));
        event_content.formatted = value.formatted_caption.map(Into::into);
        event_content.filename = filename;
        event_content
    }
}

impl TryFrom<RumaImageMessageEventContent> for ImageMessageContent {
    type Error = ClientError;

    fn try_from(value: RumaImageMessageEventContent) -> Result<Self, Self::Error> {
        Ok(Self {
            filename: value.filename().to_owned(),
            caption: value.caption().map(ToString::to_string),
            formatted_caption: value.formatted_caption().map(Into::into),
            source: Arc::new(value.source.try_into()?),
            info: value.info.as_deref().map(TryInto::try_into).transpose()?,
        })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct AudioMessageContent {
    /// The computed filename, for use in a client.
    pub filename: String,
    pub caption: Option<String>,
    pub formatted_caption: Option<FormattedBody>,
    pub source: Arc<MediaSource>,
    pub info: Option<AudioInfo>,
    pub audio: Option<UnstableAudioDetailsContent>,
    pub voice: Option<UnstableVoiceContent>,
}

impl From<AudioMessageContent> for RumaAudioMessageEventContent {
    fn from(value: AudioMessageContent) -> Self {
        let (body, filename) = get_body_and_filename(value.filename, value.caption);
        let mut event_content = Self::new(body, (*value.source).clone().into())
            .info(value.info.map(Into::into).map(Box::new));
        event_content.formatted = value.formatted_caption.map(Into::into);
        event_content.filename = filename;
        event_content.audio = value.audio.map(Into::into);
        event_content.voice = value.voice.map(Into::into);
        event_content
    }
}

impl TryFrom<RumaAudioMessageEventContent> for AudioMessageContent {
    type Error = ClientError;

    fn try_from(value: RumaAudioMessageEventContent) -> Result<Self, Self::Error> {
        Ok(Self {
            filename: value.filename().to_owned(),
            caption: value.caption().map(ToString::to_string),
            formatted_caption: value.formatted_caption().map(Into::into),
            source: Arc::new(value.source.try_into()?),
            info: value.info.as_deref().map(Into::into),
            audio: value.audio.map(Into::into),
            voice: value.voice.map(Into::into),
        })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct VideoMessageContent {
    /// The computed filename, for use in a client.
    pub filename: String,
    pub caption: Option<String>,
    pub formatted_caption: Option<FormattedBody>,
    pub source: Arc<MediaSource>,
    pub info: Option<VideoInfo>,
}

impl From<VideoMessageContent> for RumaVideoMessageEventContent {
    fn from(value: VideoMessageContent) -> Self {
        let (body, filename) = get_body_and_filename(value.filename, value.caption);
        let mut event_content = Self::new(body, (*value.source).clone().into())
            .info(value.info.map(Into::into).map(Box::new));
        event_content.formatted = value.formatted_caption.map(Into::into);
        event_content.filename = filename;
        event_content
    }
}

impl TryFrom<RumaVideoMessageEventContent> for VideoMessageContent {
    type Error = ClientError;

    fn try_from(value: RumaVideoMessageEventContent) -> Result<Self, Self::Error> {
        Ok(Self {
            filename: value.filename().to_owned(),
            caption: value.caption().map(ToString::to_string),
            formatted_caption: value.formatted_caption().map(Into::into),
            source: Arc::new(value.source.try_into()?),
            info: value.info.as_deref().map(TryInto::try_into).transpose()?,
        })
    }
}

#[derive(Clone, uniffi::Record)]
pub struct FileMessageContent {
    /// The computed filename, for use in a client.
    pub filename: String,
    pub caption: Option<String>,
    pub formatted_caption: Option<FormattedBody>,
    pub source: Arc<MediaSource>,
    pub info: Option<FileInfo>,
}

impl From<FileMessageContent> for RumaFileMessageEventContent {
    fn from(value: FileMessageContent) -> Self {
        let (body, filename) = get_body_and_filename(value.filename, value.caption);
        let mut event_content = Self::new(body, (*value.source).clone().into())
            .info(value.info.map(Into::into).map(Box::new));
        event_content.formatted = value.formatted_caption.map(Into::into);
        event_content.filename = filename;
        event_content
    }
}

impl TryFrom<RumaFileMessageEventContent> for FileMessageContent {
    type Error = ClientError;

    fn try_from(value: RumaFileMessageEventContent) -> Result<Self, Self::Error> {
        Ok(Self {
            filename: value.filename().to_owned(),
            caption: value.caption().map(ToString::to_string),
            formatted_caption: value.formatted_caption().map(Into::into),
            source: Arc::new(value.source.try_into()?),
            info: value.info.as_deref().map(TryInto::try_into).transpose()?,
        })
    }
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
    pub is_animated: Option<bool>,
}

impl From<ImageInfo> for RumaImageInfo {
    fn from(value: ImageInfo) -> Self {
        assign!(RumaImageInfo::new(), {
            height: value.height.map(u64_to_uint),
            width: value.width.map(u64_to_uint),
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
            thumbnail_info: value.thumbnail_info.map(Into::into).map(Box::new),
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone().into()),
            blurhash: value.blurhash,
            is_animated: value.is_animated,
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
            is_animated: value.is_animated,
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

        Ok(BaseAudioInfo { duration: Some(duration), size: Some(size), waveform: None })
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

impl From<UnstableAudioDetailsContent> for RumaUnstableAudioDetailsContentBlock {
    fn from(details: UnstableAudioDetailsContent) -> Self {
        Self::new(
            details.duration,
            details.waveform.iter().map(|x| UnstableAmplitude::new(x.to_owned())).collect(),
        )
    }
}

#[derive(Clone, uniffi::Record)]
pub struct UnstableVoiceContent {}

impl From<RumaUnstableVoiceContentBlock> for UnstableVoiceContent {
    fn from(_details: RumaUnstableVoiceContentBlock) -> Self {
        Self {}
    }
}

impl From<UnstableVoiceContent> for RumaUnstableVoiceContentBlock {
    fn from(_details: UnstableVoiceContent) -> Self {
        Self::new()
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
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone().into()),
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
            thumbnail_source: value.thumbnail_source.map(|source| (*source).clone().into()),
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

impl TryFrom<&matrix_sdk::ruma::events::room::ImageInfo> for ImageInfo {
    type Error = ClientError;

    fn try_from(info: &matrix_sdk::ruma::events::room::ImageInfo) -> Result<Self, Self::Error> {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Ok(Self {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info
                .thumbnail_source
                .as_ref()
                .map(TryInto::try_into)
                .transpose()?
                .map(Arc::new),
            blurhash: info.blurhash.clone(),
            is_animated: info.is_animated,
        })
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

impl TryFrom<&RumaVideoInfo> for VideoInfo {
    type Error = ClientError;

    fn try_from(info: &RumaVideoInfo) -> Result<Self, Self::Error> {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Ok(Self {
            duration: info.duration,
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info
                .thumbnail_source
                .as_ref()
                .map(TryInto::try_into)
                .transpose()?
                .map(Arc::new),
            blurhash: info.blurhash.clone(),
        })
    }
}

impl TryFrom<&RumaFileInfo> for FileInfo {
    type Error = ClientError;

    fn try_from(info: &RumaFileInfo) -> Result<Self, Self::Error> {
        let thumbnail_info = info.thumbnail_info.as_ref().map(|info| ThumbnailInfo {
            height: info.height.map(Into::into),
            width: info.width.map(Into::into),
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
        });

        Ok(Self {
            mimetype: info.mimetype.clone(),
            size: info.size.map(Into::into),
            thumbnail_info,
            thumbnail_source: info
                .thumbnail_source
                .as_ref()
                .map(TryInto::try_into)
                .transpose()?
                .map(Arc::new),
        })
    }
}

#[derive(Clone, uniffi::Enum)]
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

/// Creates a [`RoomMessageEventContentWithoutRelation`] given a
/// [`MessageContent`] value.
#[matrix_sdk_ffi_macros::export]
pub fn content_without_relation_from_message(
    message: MessageContent,
) -> Result<Arc<RoomMessageEventContentWithoutRelation>, ClientError> {
    let msg_type = message.msg_type.try_into()?;
    Ok(Arc::new(RoomMessageEventContentWithoutRelation::new(msg_type)))
}

/// Types of global account data events.
#[derive(Clone, uniffi::Enum)]
pub enum AccountDataEventType {
    /// m.direct
    Direct,
    /// m.identity_server
    IdentityServer,
    /// m.ignored_user_list
    IgnoredUserList,
    /// m.push_rules
    PushRules,
    /// m.secret_storage.default_key
    SecretStorageDefaultKey,
    /// m.secret_storage.key.*
    SecretStorageKey { key_id: String },
}

impl TryFrom<RumaGlobalAccountDataEventType> for AccountDataEventType {
    type Error = String;

    fn try_from(value: RumaGlobalAccountDataEventType) -> Result<Self, Self::Error> {
        match value {
            RumaGlobalAccountDataEventType::Direct => Ok(Self::Direct),
            RumaGlobalAccountDataEventType::IdentityServer => Ok(Self::IdentityServer),
            RumaGlobalAccountDataEventType::IgnoredUserList => Ok(Self::IgnoredUserList),
            RumaGlobalAccountDataEventType::PushRules => Ok(Self::PushRules),
            RumaGlobalAccountDataEventType::SecretStorageDefaultKey => {
                Ok(Self::SecretStorageDefaultKey)
            }
            RumaGlobalAccountDataEventType::SecretStorageKey(key_id) => {
                Ok(Self::SecretStorageKey { key_id })
            }
            _ => Err("Unsupported account data event type".to_owned()),
        }
    }
}

/// Global account data events.
#[derive(Clone, uniffi::Enum)]
pub enum AccountDataEvent {
    /// m.direct
    Direct {
        /// The mapping of user ID to a list of room IDs of the ‘direct’ rooms
        /// for that user ID.
        map: HashMap<String, Vec<String>>,
    },
    /// m.identity_server
    IdentityServer {
        /// The base URL for the identity server for client-server connections.
        base_url: Option<String>,
    },
    /// m.ignored_user_list
    IgnoredUserList {
        /// The map of users to ignore. This is a mapping of user ID to empty
        /// object.
        ignored_users: HashMap<String, IgnoredUser>,
    },
    /// m.push_rules
    PushRules {
        /// The global ruleset.
        global: Ruleset,
    },
    /// m.secret_storage.default_key
    SecretStorageDefaultKey {
        /// The ID of the default key.
        key_id: String,
    },
    /// m.secret_storage.key.*
    SecretStorageKey {
        /// The ID of the key.
        key_id: String,

        /// The name of the key.
        name: Option<String>,

        /// The encryption algorithm used for this key.
        ///
        /// Currently, only `m.secret_storage.v1.aes-hmac-sha2` is supported.
        algorithm: SecretStorageEncryptionAlgorithm,

        /// The passphrase from which to generate the key.
        passphrase: Option<PassPhrase>,
    },
}

/// The policy that decides if media previews should be shown in the timeline.
#[derive(Clone, uniffi::Enum, Default)]
pub enum MediaPreviews {
    /// Always show media previews in the timeline.
    #[default]
    On,
    /// Show media previews in the timeline only if the room is private.
    Private,
    /// Never show media previews in the timeline.
    Off,
}

impl From<RumaMediaPreviews> for MediaPreviews {
    fn from(value: RumaMediaPreviews) -> Self {
        match value {
            RumaMediaPreviews::On => Self::On,
            RumaMediaPreviews::Private => Self::Private,
            RumaMediaPreviews::Off => Self::Off,
            _ => Default::default(),
        }
    }
}

impl From<MediaPreviews> for RumaMediaPreviews {
    fn from(value: MediaPreviews) -> Self {
        match value {
            MediaPreviews::On => Self::On,
            MediaPreviews::Private => Self::Private,
            MediaPreviews::Off => Self::Off,
        }
    }
}

/// The policy that decides if avatars should be shown in invite requests.
#[derive(Clone, uniffi::Enum, Default)]
pub enum InviteAvatars {
    /// Always show avatars in invite requests.
    #[default]
    On,
    /// Never show avatars in invite requests.
    Off,
}

impl From<RumaInviteAvatars> for InviteAvatars {
    fn from(value: RumaInviteAvatars) -> Self {
        match value {
            RumaInviteAvatars::On => Self::On,
            RumaInviteAvatars::Off => Self::Off,
            _ => Default::default(),
        }
    }
}

impl From<InviteAvatars> for RumaInviteAvatars {
    fn from(value: InviteAvatars) -> Self {
        match value {
            InviteAvatars::On => Self::On,
            InviteAvatars::Off => Self::Off,
        }
    }
}

/// Details about an ignored user.
///
/// This is currently empty.
#[derive(Clone, uniffi::Record)]
pub struct IgnoredUser {}

impl From<RumaIgnoredUser> for IgnoredUser {
    fn from(_value: RumaIgnoredUser) -> Self {
        IgnoredUser {}
    }
}

/// A push ruleset scopes a set of rules according to some criteria.
#[derive(Clone, uniffi::Record)]
pub struct Ruleset {
    /// These rules configure behavior for (unencrypted) messages that match
    /// certain patterns.
    pub content: Vec<PatternedPushRule>,

    /// These user-configured rules are given the highest priority.
    ///
    /// This field is named `override_` instead of `override` because the latter
    /// is a reserved keyword in Rust.
    pub override_: Vec<ConditionalPushRule>,

    /// These rules change the behavior of all messages for a given room.
    pub room: Vec<SimplePushRule>,

    /// These rules configure notification behavior for messages from a specific
    /// Matrix user ID.
    pub sender: Vec<SimplePushRule>,

    /// These rules are identical to override rules, but have a lower priority
    /// than `content`, `room` and `sender` rules.
    pub underride: Vec<ConditionalPushRule>,
}

impl TryFrom<RumaRuleset> for Ruleset {
    type Error = String;

    fn try_from(value: RumaRuleset) -> Result<Self, Self::Error> {
        Ok(Self {
            content: value
                .content
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            override_: value
                .override_
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            room: value.room.into_iter().map(TryInto::try_into).collect::<Result<Vec<_>, _>>()?,
            sender: value
                .sender
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            underride: value
                .underride
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

/// Like [`SimplePushRule`], but with an additional `pattern`` field.
#[derive(Clone, uniffi::Record)]
pub struct PatternedPushRule {
    /// Actions to determine if and how a notification is delivered for events
    /// matching this rule.
    pub actions: Vec<Action>,

    /// Whether this is a default rule, or has been set explicitly.
    pub default: bool,

    /// Whether the push rule is enabled or not.
    pub enabled: bool,

    /// The ID of this rule.
    pub rule_id: String,

    /// The glob-style pattern to match against.
    pub pattern: String,
}

impl TryFrom<RumaPatternedPushRule> for PatternedPushRule {
    type Error = String;

    fn try_from(value: RumaPatternedPushRule) -> Result<Self, Self::Error> {
        Ok(Self {
            actions: value
                .actions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            default: value.default,
            enabled: value.enabled,
            rule_id: value.rule_id,
            pattern: value.pattern,
        })
    }
}

/// Like [`SimplePushRule`], but with an additional `conditions` field.
#[derive(Clone, uniffi::Record)]
pub struct ConditionalPushRule {
    /// Actions to determine if and how a notification is delivered for events
    /// matching this rule.
    pub actions: Vec<Action>,

    /// Whether this is a default rule, or has been set explicitly.
    pub default: bool,

    /// Whether the push rule is enabled or not.
    pub enabled: bool,

    /// The ID of this rule.
    pub rule_id: String,

    /// The conditions that must hold true for an event in order for a rule to
    /// be applied to an event.
    ///
    /// A rule with no conditions always matches.
    pub conditions: Vec<PushCondition>,
}

impl TryFrom<RumaConditionalPushRule> for ConditionalPushRule {
    type Error = String;

    fn try_from(value: RumaConditionalPushRule) -> Result<Self, Self::Error> {
        Ok(Self {
            actions: value
                .actions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            default: value.default,
            enabled: value.enabled,
            rule_id: value.rule_id,
            conditions: value
                .conditions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

/// A push rule is a single rule that states under what conditions an event
/// should be passed onto a push gateway and how the notification should be
/// presented.
#[derive(Clone, uniffi::Record)]
pub struct SimplePushRule {
    /// Actions to determine if and how a notification is delivered for events
    /// matching this rule.
    pub actions: Vec<Action>,

    /// Whether this is a default rule, or has been set explicitly.
    pub default: bool,

    /// Whether the push rule is enabled or not.
    pub enabled: bool,

    /// The ID of this rule.
    ///
    /// This is generally the Matrix ID of the entity that it applies to.
    pub rule_id: String,
}

impl TryFrom<RumaSimplePushRule<OwnedRoomId>> for SimplePushRule {
    type Error = String;

    fn try_from(value: RumaSimplePushRule<OwnedRoomId>) -> Result<Self, Self::Error> {
        Ok(Self {
            actions: value
                .actions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            default: value.default,
            enabled: value.enabled,
            rule_id: value.rule_id.into(),
        })
    }
}

impl TryFrom<RumaSimplePushRule<OwnedUserId>> for SimplePushRule {
    type Error = String;

    fn try_from(value: RumaSimplePushRule<OwnedUserId>) -> Result<Self, Self::Error> {
        Ok(Self {
            actions: value
                .actions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            default: value.default,
            enabled: value.enabled,
            rule_id: value.rule_id.into(),
        })
    }
}

/// An algorithm and its properties, used to encrypt a secret.
#[derive(Clone, uniffi::Enum)]
pub enum SecretStorageEncryptionAlgorithm {
    /// Encrypted using the `m.secret_storage.v1.aes-hmac-sha2` algorithm.
    ///
    /// Secrets using this method are encrypted using AES-CTR-256 and
    /// authenticated using HMAC-SHA-256.
    V1AesHmacSha2 { properties: SecretStorageV1AesHmacSha2Properties },
}

impl TryFrom<RumaSecretStorageEncryptionAlgorithm> for SecretStorageEncryptionAlgorithm {
    type Error = String;

    fn try_from(value: RumaSecretStorageEncryptionAlgorithm) -> Result<Self, Self::Error> {
        match value {
            RumaSecretStorageEncryptionAlgorithm::V1AesHmacSha2(properties) => {
                Ok(Self::V1AesHmacSha2 { properties: properties.into() })
            }
            _ => Err("Unsupported encryption algorithm".to_owned()),
        }
    }
}

/// The key properties for the `m.secret_storage.v1.aes-hmac-sha2`` algorithm.
#[derive(Clone, uniffi::Record)]
pub struct SecretStorageV1AesHmacSha2Properties {
    /// The 16-byte initialization vector, encoded as base64.
    pub iv: Option<String>,

    /// The MAC, encoded as base64.
    pub mac: Option<String>,
}

impl From<RumaSecretStorageV1AesHmacSha2Properties> for SecretStorageV1AesHmacSha2Properties {
    fn from(value: RumaSecretStorageV1AesHmacSha2Properties) -> Self {
        Self {
            iv: value.iv.map(|base64| base64.encode()),
            mac: value.mac.map(|base64| base64.encode()),
        }
    }
}

/// The content of an `m.media_preview_config` event.
///
/// Is also the content of the unstable
/// `io.element.msc4278.media_preview_config`.
#[derive(Clone, uniffi::Record, Default)]
pub struct MediaPreviewConfig {
    /// The media previews setting for the user.
    pub media_previews: Option<MediaPreviews>,

    /// The invite avatars setting for the user.
    pub invite_avatars: Option<InviteAvatars>,
}

impl From<MediaPreviewConfigEventContent> for MediaPreviewConfig {
    fn from(value: MediaPreviewConfigEventContent) -> Self {
        Self {
            media_previews: value.media_previews.map(Into::into),
            invite_avatars: value.invite_avatars.map(Into::into),
        }
    }
}

/// A passphrase from which a key is to be derived.
#[derive(Clone, uniffi::Record)]
pub struct PassPhrase {
    /// The algorithm to use to generate the key from the passphrase.
    ///
    /// Must be `m.pbkdf2`.
    pub algorithm: KeyDerivationAlgorithm,

    /// The salt used in PBKDF2.
    pub salt: String,

    /// The number of iterations to use in PBKDF2.
    pub iterations: u64,

    /// The number of bits to generate for the key.
    ///
    /// Defaults to 256
    pub bits: u64,
}

impl TryFrom<RumaPassPhrase> for PassPhrase {
    type Error = String;

    fn try_from(value: RumaPassPhrase) -> Result<Self, Self::Error> {
        Ok(PassPhrase {
            algorithm: value.algorithm.try_into()?,
            salt: value.salt,
            iterations: value.iterations.into(),
            bits: value.bits.into(),
        })
    }
}

/// A key algorithm to be used to generate a key from a passphrase.
#[derive(Clone, uniffi::Enum)]
pub enum KeyDerivationAlgorithm {
    /// PBKDF2
    Pbkfd2,
}

impl TryFrom<RumaKeyDerivationAlgorithm> for KeyDerivationAlgorithm {
    type Error = String;

    fn try_from(value: RumaKeyDerivationAlgorithm) -> Result<Self, Self::Error> {
        match value {
            RumaKeyDerivationAlgorithm::Pbkfd2 => Ok(Self::Pbkfd2),
            _ => Err("Unsupported key derivation algorithm".to_owned()),
        }
    }
}

impl From<RumaGlobalAccountDataEvent<DirectEventContent>> for AccountDataEvent {
    fn from(value: RumaGlobalAccountDataEvent<DirectEventContent>) -> Self {
        Self::Direct {
            map: value
                .content
                .0
                .into_iter()
                .map(|(user_id, room_ids)| {
                    (user_id.to_string(), room_ids.iter().map(ToString::to_string).collect())
                })
                .collect(),
        }
    }
}

impl From<RumaGlobalAccountDataEvent<IdentityServerEventContent>> for AccountDataEvent {
    fn from(value: RumaGlobalAccountDataEvent<IdentityServerEventContent>) -> Self {
        Self::IdentityServer { base_url: value.content.base_url.into_option() }
    }
}

impl From<RumaGlobalAccountDataEvent<IgnoredUserListEventContent>> for AccountDataEvent {
    fn from(value: RumaGlobalAccountDataEvent<IgnoredUserListEventContent>) -> Self {
        Self::IgnoredUserList {
            ignored_users: value
                .content
                .ignored_users
                .into_iter()
                .map(|(user_id, ignored_user)| {
                    (user_id.to_string(), IgnoredUser::from(ignored_user))
                })
                .collect(),
        }
    }
}

impl TryFrom<RumaGlobalAccountDataEvent<PushRulesEventContent>> for AccountDataEvent {
    type Error = String;

    fn try_from(
        value: RumaGlobalAccountDataEvent<PushRulesEventContent>,
    ) -> Result<Self, Self::Error> {
        Ok(Self::PushRules { global: value.content.global.try_into()? })
    }
}

impl From<RumaGlobalAccountDataEvent<SecretStorageDefaultKeyEventContent>> for AccountDataEvent {
    fn from(value: RumaGlobalAccountDataEvent<SecretStorageDefaultKeyEventContent>) -> Self {
        Self::SecretStorageDefaultKey { key_id: value.content.key_id }
    }
}

impl TryFrom<RumaGlobalAccountDataEvent<SecretStorageKeyEventContent>> for AccountDataEvent {
    type Error = String;

    fn try_from(
        value: RumaGlobalAccountDataEvent<SecretStorageKeyEventContent>,
    ) -> Result<Self, Self::Error> {
        Ok(Self::SecretStorageKey {
            key_id: value.content.key_id,
            name: value.content.name,
            algorithm: value.content.algorithm.try_into()?,
            passphrase: value.content.passphrase.map(TryInto::try_into).transpose()?,
        })
    }
}

/// Types of room account data events.
#[derive(Clone, uniffi::Enum)]
pub enum RoomAccountDataEventType {
    /// m.fully_read
    FullyRead,
    /// m.marked_unread
    MarkedUnread,
    /// m.tag
    Tag,
    /// com.famedly.marked_unread
    UnstableMarkedUnread,
}

impl TryFrom<RumaRoomAccountDataEventType> for RoomAccountDataEventType {
    type Error = String;

    fn try_from(value: RumaRoomAccountDataEventType) -> Result<Self, Self::Error> {
        match value {
            RumaRoomAccountDataEventType::FullyRead => Ok(Self::FullyRead),
            RumaRoomAccountDataEventType::MarkedUnread => Ok(Self::MarkedUnread),
            RumaRoomAccountDataEventType::Tag => Ok(Self::Tag),
            RumaRoomAccountDataEventType::UnstableMarkedUnread => Ok(Self::UnstableMarkedUnread),
            _ => Err("Unsupported account data event type".to_owned()),
        }
    }
}

/// Room account data events.
#[derive(Clone, uniffi::Enum)]
pub enum RoomAccountDataEvent {
    /// m.fully_read
    FullyReadEvent {
        /// The event the user's read marker is located at in the room.
        event_id: String,
    },
    /// m.marked_unread
    MarkedUnread {
        /// The current unread state.
        unread: bool,
    },
    /// m.tag
    Tag { tags: HashMap<TagName, TagInfo> },
    /// com.famedly.marked_unread
    UnstableMarkedUnread {
        /// The current unread state.
        unread: bool,
    },
}

/// The name of a tag.
#[derive(Clone, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum TagName {
    /// `m.favourite`: The user's favorite rooms.
    Favorite,

    /// `m.lowpriority`: These should be shown with lower precedence than
    /// others.
    LowPriority,

    /// `m.server_notice`: Used to identify
    ServerNotice,

    /// `u.*`: User-defined tag
    User { name: UserTagName },
}

impl TryFrom<RumaTagName> for TagName {
    type Error = String;

    fn try_from(value: RumaTagName) -> Result<Self, Self::Error> {
        match value {
            RumaTagName::Favorite => Ok(Self::Favorite),
            RumaTagName::LowPriority => Ok(Self::LowPriority),
            RumaTagName::ServerNotice => Ok(Self::ServerNotice),
            RumaTagName::User(name) => Ok(Self::User { name: name.into() }),
            _ => Err("Unsupported tag name".to_owned()),
        }
    }
}

/// A user-defined tag name.
#[derive(Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct UserTagName {
    name: String,
}

impl From<RumaUserTagName> for UserTagName {
    fn from(value: RumaUserTagName) -> Self {
        Self { name: value.as_ref().to_owned() }
    }
}

/// Information about a tag.
#[derive(Clone, uniffi::Record)]
pub struct TagInfo {
    /// Value to use for lexicographically ordering rooms with this tag.
    pub order: Option<f64>,
}

impl From<RumaTagInfo> for TagInfo {
    fn from(value: RumaTagInfo) -> Self {
        Self { order: value.order }
    }
}

impl From<RumaRoomAccountDataEvent<FullyReadEventContent>> for RoomAccountDataEvent {
    fn from(value: RumaRoomAccountDataEvent<FullyReadEventContent>) -> Self {
        Self::FullyReadEvent { event_id: value.content.event_id.into() }
    }
}

impl From<RumaRoomAccountDataEvent<MarkedUnreadEventContent>> for RoomAccountDataEvent {
    fn from(value: RumaRoomAccountDataEvent<MarkedUnreadEventContent>) -> Self {
        Self::MarkedUnread { unread: value.content.unread }
    }
}

impl TryFrom<RumaRoomAccountDataEvent<TagEventContent>> for RoomAccountDataEvent {
    type Error = String;

    fn try_from(value: RumaRoomAccountDataEvent<TagEventContent>) -> Result<Self, Self::Error> {
        Ok(Self::Tag {
            tags: value
                .content
                .tags
                .into_iter()
                .map(|(name, info)| name.try_into().map(|name| (name, info.into())))
                .collect::<Result<HashMap<TagName, _>, _>>()?,
        })
    }
}

impl From<RumaRoomAccountDataEvent<UnstableMarkedUnreadEventContent>> for RoomAccountDataEvent {
    fn from(value: RumaRoomAccountDataEvent<UnstableMarkedUnreadEventContent>) -> Self {
        Self::UnstableMarkedUnread { unread: value.content.unread }
    }
}

#[cfg(feature = "unstable-msc4274")]
pub use galleries::*;

#[cfg(feature = "unstable-msc4274")]
mod galleries {
    use ruma::{
        events::room::message::{
            GalleryItemType as RumaGalleryItemType,
            GalleryMessageEventContent as RumaGalleryMessageEventContent,
        },
        serde::JsonObject,
    };

    use crate::{
        error::ClientError,
        ruma::{
            AudioMessageContent, FileMessageContent, FormattedBody, ImageMessageContent,
            VideoMessageContent,
        },
    };

    #[derive(Clone, uniffi::Record)]
    pub struct GalleryMessageContent {
        pub body: String,
        pub formatted: Option<FormattedBody>,
        pub itemtypes: Vec<GalleryItemType>,
    }

    impl TryFrom<GalleryMessageContent> for RumaGalleryMessageEventContent {
        type Error = ClientError;

        fn try_from(value: GalleryMessageContent) -> Result<Self, Self::Error> {
            Ok(Self::new(
                value.body,
                value.formatted.map(Into::into),
                value.itemtypes.into_iter().map(TryInto::try_into).collect::<Result<_, _>>()?,
            ))
        }
    }

    impl TryFrom<RumaGalleryMessageEventContent> for GalleryMessageContent {
        type Error = ClientError;

        fn try_from(value: RumaGalleryMessageEventContent) -> Result<Self, Self::Error> {
            Ok(Self {
                body: value.body,
                formatted: value.formatted.as_ref().map(Into::into),
                itemtypes: value
                    .itemtypes
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            })
        }
    }

    #[derive(Clone, uniffi::Enum)]
    pub enum GalleryItemType {
        Image { content: ImageMessageContent },
        Audio { content: AudioMessageContent },
        Video { content: VideoMessageContent },
        File { content: FileMessageContent },
        Other { itemtype: String, body: String },
    }

    impl TryFrom<GalleryItemType> for RumaGalleryItemType {
        type Error = ClientError;

        fn try_from(value: GalleryItemType) -> Result<Self, Self::Error> {
            Ok(match value {
                GalleryItemType::Image { content } => Self::Image(content.into()),
                GalleryItemType::Audio { content } => Self::Audio(content.into()),
                GalleryItemType::Video { content } => Self::Video(content.into()),
                GalleryItemType::File { content } => Self::File(content.into()),
                GalleryItemType::Other { itemtype, body } => {
                    Self::new(&itemtype, body, JsonObject::default())?
                }
            })
        }
    }

    impl TryFrom<RumaGalleryItemType> for GalleryItemType {
        type Error = ClientError;

        fn try_from(value: RumaGalleryItemType) -> Result<Self, Self::Error> {
            Ok(match value {
                RumaGalleryItemType::Image(c) => GalleryItemType::Image { content: c.try_into()? },
                RumaGalleryItemType::Audio(c) => GalleryItemType::Audio { content: c.try_into()? },
                RumaGalleryItemType::Video(c) => GalleryItemType::Video { content: c.try_into()? },
                RumaGalleryItemType::File(c) => GalleryItemType::File { content: c.try_into()? },
                _ => GalleryItemType::Other {
                    itemtype: value.itemtype().to_owned(),
                    body: value.body().to_owned(),
                },
            })
        }
    }
}
