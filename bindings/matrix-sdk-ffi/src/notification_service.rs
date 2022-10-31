use std::sync::Arc;

use matrix_sdk::{
    room::timeline::{
        BundledReactions, EventTimelineItem, Message, TimelineItemContent, TimelineKey,
    },
    ruma::{
        api::client::message,
        events::room::{
            message::{ImageMessageEventContent, MessageType, TextMessageEventContent},
            ImageInfo,
        },
        EventId, MilliSecondsSinceUnixEpoch, MxcUri, ServerName, UInt, UserId,
    },
};
use rand::{random, Rng};

use crate::{ClientError, TimelineItem};

pub struct NotificationService {
    base_path: String,
    user_id: String,
}

pub struct NotificationItem {
    pub item: Arc<TimelineItem>,
    pub title: String,
    pub subtitle: Option<String>,
    pub is_noisy: bool,
    pub avatar_url: Option<String>,
}

impl NotificationService {
    /// Creates a new notification service.
    pub fn new(base_path: String, user_id: String) -> Self {
        NotificationService { base_path, user_id }
    }

    pub fn get_notification_item(
        &self,
        _room_id: String,
        event_id: String,
    ) -> anyhow::Result<Option<NotificationItem>> {
        let text_message_type = MessageType::Text(TextMessageEventContent::plain("Notified text"));
        let mut image_info = ImageInfo::new();
        image_info.height = UInt::new(640);
        image_info.width = UInt::new(638);
        image_info.mimetype = Some("image/jpeg".to_owned());
        image_info.size = UInt::new(118141);
        image_info.blurhash = Some("TFF~Ba5at616yD^%~T-oXU9bt6kr".to_owned());
        let image_message_type = MessageType::Image(ImageMessageEventContent::plain(
            "body".to_owned(),
            (*Box::<MxcUri>::from("mxc://matrix.org/vNOdmUTIPIwWvMneNzyCzNhb")).to_owned(),
            Some(Box::new(image_info)),
        ));

        let random = rand::thread_rng().gen_range(0..2);

        let sdk_event = EventTimelineItem {
            key: TimelineKey::EventId(EventId::parse(event_id.to_owned()).unwrap()),
            event_id: Some(EventId::parse(event_id.to_owned()).unwrap()),
            sender: UserId::parse("@username:example.com").unwrap(),
            content: TimelineItemContent::Message(Message {
                msgtype: match random {
                    1 => image_message_type,
                    _ => text_message_type,
                },
                in_reply_to: None,
                edited: false,
            }),
            reactions: BundledReactions::default(),
            origin_server_ts: Some(MilliSecondsSinceUnixEpoch::now()),
            is_own: false,
            encryption_info: None,
            raw: None,
        };

        let item = NotificationItem {
            item: Arc::new(TimelineItem(matrix_sdk::room::timeline::TimelineItem::Event(
                sdk_event,
            ))),
            title: "ismailgulek".to_owned(),
            subtitle: Some("Element iOS - Internal".to_owned()),
            is_noisy: true,
            avatar_url: Some("mxc://matrix.org/XzNaquIfpvjHtftUJBCRNDgX".to_owned()),
        };
        Ok(Some(item))
    }
}
