use std::sync::Arc;

use matrix_sdk::{
    room::timeline::{BundledReactions, EventTimelineItem, Message, TimelineItemContent},
    ruma::{
        events::room::{
            message::{ImageMessageEventContent, MessageType, TextMessageEventContent},
            ImageInfo,
        },
        EventId, MilliSecondsSinceUnixEpoch, MxcUri, UInt, UserId,
    },
};
use rand::Rng;

use crate::TimelineItem;

#[allow(dead_code)]
pub struct NotificationService {
    base_path: String,
    user_id: String,
}

/// Notification item struct.
pub struct NotificationItem {
    /// Actual timeline item for the event sent.
    pub item: Arc<TimelineItem>,
    /// Title of the notification. Usually would be event sender's display name.
    pub title: String,
    /// Subtitle of the notification. Usually would be the room name for
    /// non-direct rooms, and none for direct rooms.
    pub subtitle: Option<String>,
    /// Flag indicating the notification should play a sound.
    pub is_noisy: bool,
    /// Avatar url of the room the event sent to (if any).
    pub avatar_url: Option<String>,
}

impl NotificationService {
    /// Creates a new notification service.
    /// Will be used to fetch an event after receiving a notification.
    /// Please note that this will be called on a new process than the
    /// application context.
    pub fn new(base_path: String, user_id: String) -> Self {
        NotificationService { base_path, user_id }
    }

    /// Get notification item for a given room_id and event_id.
    /// Returns none if this notification should not be displayed to the user.
    pub fn get_notification_item(
        &self,
        room_id: String,
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
