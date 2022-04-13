use std::sync::Arc;

use extension_trait::extension_trait;
pub use matrix_sdk::ruma::events::room::{
    message::RoomMessageEventContent as MessageEventContent, MediaSource,
};
use matrix_sdk::{
    deserialized_responses::SyncRoomEvent,
    ruma::events::{
        room::{
            message::{ImageMessageEventContent, MessageFormat, MessageType},
            ImageInfo,
        },
        AnySyncMessageLikeEvent, AnySyncRoomEvent, SyncMessageLikeEvent,
    },
};

#[derive(Clone)]
pub struct BaseMessage {
    id: String,
    body: String,
    sender: String,
    origin_server_ts: u64,
    transaction_id: Option<String>,
}

impl BaseMessage {
    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub fn body(&self) -> String {
        self.body.clone()
    }

    pub fn sender(&self) -> String {
        self.sender.clone()
    }

    pub fn origin_server_ts(&self) -> u64 {
        self.origin_server_ts
    }

    pub fn transaction_id(&self) -> Option<String> {
        self.transaction_id.clone()
    }
}

pub struct TextMessage {
    base_message: Arc<BaseMessage>,
    html_body: Option<String>,
}

impl TextMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        self.base_message.clone()
    }

    pub fn html_body(&self) -> Option<String> {
        self.html_body.clone()
    }
}

pub struct ImageMessage {
    base_message: Arc<BaseMessage>,
    source: Arc<MediaSource>,
    info: Option<Box<ImageInfo>>,
}

impl ImageMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        self.base_message.clone()
    }

    pub fn source(&self) -> Arc<MediaSource> {
        self.source.clone()
    }

    pub fn width(&self) -> Option<u64> {
        self.info.clone()?.width?.try_into().ok()
    }

    pub fn height(&self) -> Option<u64> {
        self.info.clone()?.height?.try_into().ok()
    }

    pub fn blurhash(&self) -> Option<String> {
        self.info.clone()?.blurhash
    }
}

pub struct NoticeMessage {
    base_message: Arc<BaseMessage>,
    html_body: Option<String>,
}

impl NoticeMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        self.base_message.clone()
    }

    pub fn html_body(&self) -> Option<String> {
        self.html_body.clone()
    }
}

pub struct EmoteMessage {
    base_message: Arc<BaseMessage>,
    html_body: Option<String>,
}

impl EmoteMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        self.base_message.clone()
    }

    pub fn html_body(&self) -> Option<String> {
        self.html_body.clone()
    }
}

pub struct AnyMessage {
    text: Option<Arc<TextMessage>>,
    image: Option<Arc<ImageMessage>>,
    notice: Option<Arc<NoticeMessage>>,
    emote: Option<Arc<EmoteMessage>>,
}

impl AnyMessage {
    pub fn text_message(&self) -> Option<Arc<TextMessage>> {
        self.text.clone()
    }

    pub fn image_message(&self) -> Option<Arc<ImageMessage>> {
        self.image.clone()
    }

    pub fn notice_message(&self) -> Option<Arc<NoticeMessage>> {
        self.notice.clone()
    }

    pub fn emote_message(&self) -> Option<Arc<EmoteMessage>> {
        self.emote.clone()
    }
}

pub fn sync_event_to_message(sync_event: SyncRoomEvent) -> Option<Arc<AnyMessage>> {
    match sync_event.event.deserialize() {
        Ok(AnySyncRoomEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(m),
        ))) => {
            let base_message = Arc::new(BaseMessage {
                id: m.event_id.to_string(),
                body: m.content.body().to_owned(),
                sender: m.sender.to_string(),
                origin_server_ts: m.origin_server_ts.as_secs().into(),
                transaction_id: m.unsigned.transaction_id.map(Into::into),
            });

            match m.content.msgtype {
                MessageType::Image(ImageMessageEventContent { source, info, .. }) => {
                    let any_message = AnyMessage {
                        text: None,
                        image: Some(Arc::new(ImageMessage {
                            base_message,
                            source: Arc::new(source),
                            info,
                        })),
                        notice: None,
                        emote: None,
                    };

                    Some(Arc::new(any_message))
                }
                MessageType::Text(content) => {
                    let mut html_body: Option<String> = None;
                    if let Some(formatted_body) = content.formatted {
                        if formatted_body.format == MessageFormat::Html {
                            html_body = Some(formatted_body.body);
                        }
                    }

                    let any_message = AnyMessage {
                        text: Some(Arc::new(TextMessage { base_message, html_body })),
                        image: None,
                        notice: None,
                        emote: None,
                    };
                    Some(Arc::new(any_message))
                }
                MessageType::Notice(content) => {
                    let mut html_body: Option<String> = None;
                    if let Some(formatted_body) = content.formatted {
                        if formatted_body.format == MessageFormat::Html {
                            html_body = Some(formatted_body.body);
                        }
                    }

                    let any_message = AnyMessage {
                        text: None,
                        image: None,
                        notice: Some(Arc::new(NoticeMessage { base_message, html_body })),
                        emote: None,
                    };
                    Some(Arc::new(any_message))
                }
                MessageType::Emote(content) => {
                    let mut html_body: Option<String> = None;
                    if let Some(formatted_body) = content.formatted {
                        if formatted_body.format == MessageFormat::Html {
                            html_body = Some(formatted_body.body);
                        }
                    }

                    let any_message = AnyMessage {
                        text: None,
                        image: None,
                        notice: None,
                        emote: Some(Arc::new(EmoteMessage { base_message, html_body })),
                    };
                    Some(Arc::new(any_message))
                }
                _ => {
                    let any_message = AnyMessage {
                        text: Some(Arc::new(TextMessage { base_message, html_body: None })),
                        image: None,
                        notice: None,
                        emote: None,
                    };
                    Some(Arc::new(any_message))
                }
            }
        }
        _ => None,
    }
}

pub fn media_source_from_url(url: String) -> Arc<MediaSource> {
    Arc::new(MediaSource::Plain(url.into()))
}

pub fn message_event_content_from_markdown(md: String) -> Arc<MessageEventContent> {
    Arc::new(MessageEventContent::text_markdown(md))
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
