use std::sync::Arc;

use matrix_sdk::{
    deserialized_responses::SyncRoomEvent,
    ruma::{
        events::{
            room::{
                MediaSource,
                message::{MessageType, MessageFormat, ImageMessageEventContent},
            },
            AnySyncMessageLikeEvent, AnySyncRoomEvent,
        },
        MxcUri,
    },
};

#[derive(Clone)]
pub struct BaseMessage {
    id: String,
    body: String,
    sender: String,
    origin_server_ts: u64,
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
        self.origin_server_ts.clone()
    }
}

pub struct TextMessage {
    base_message: Arc<BaseMessage>,
    html_body: Option<String>,
}

impl TextMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        return self.base_message.clone();
    }

    pub fn html_body(&self) -> Option<String> {
        self.html_body.clone()
    }
}

pub struct ImageMessage {
    base_message: Arc<BaseMessage>,
    url: Option<Box<MxcUri>>,
}

impl ImageMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        return self.base_message.clone();
    }

    pub fn url(&self) -> Option<String> {
        match self.url.clone() {
            Some(url) => return Some(url.to_string()),
            _ => return None,
        }
    }
}

pub struct NoticeMessage {
    base_message: Arc<BaseMessage>,
    html_body: Option<String>,
}

impl NoticeMessage {
    pub fn base_message(&self) -> Arc<BaseMessage> {
        return self.base_message.clone();
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
        return self.base_message.clone();
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
        Ok(AnySyncRoomEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(m))) => {
            let base_message = Arc::new(BaseMessage {
                id: m.event_id.to_string(),
                body: m.content.body().to_string(),
                sender: m.sender.to_string(),
                origin_server_ts: m.origin_server_ts.as_secs().into(),
            });

            match m.content.msgtype {
                MessageType::Image(ImageMessageEventContent { source: MediaSource::Plain(url), ..}) => {
                    let any_message = AnyMessage {
                        text: None,
                        image: Some(Arc::new(ImageMessage { base_message, url: Some(url) })),
                        notice: None,
                        emote: None
                    };

                    return Some(Arc::new(any_message));
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
                        emote: None
                    };
                    return Some(Arc::new(any_message));
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
                        emote: None
                    };
                    return Some(Arc::new(any_message));
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
                        emote: Some(Arc::new(EmoteMessage { base_message, html_body }))
                    };
                    return Some(Arc::new(any_message));
                }
                _ => {
                    let any_message = AnyMessage {
                        text: Some(Arc::new(TextMessage { base_message, html_body: None })),
                        image: None,
                        notice: None,
                        emote: None
                    };
                    return Some(Arc::new(any_message));
                }
            }
        }
        _ => None,
    }
}
