use std::sync::Arc;

use matrix_sdk::{deserialized_responses::SyncRoomEvent, ruma::events::{AnySyncMessageEvent, AnySyncRoomEvent}};

pub struct Message {
    id: String,
    message_type: String,
    content: String,
    sender: String,
    origin_server_ts: u64
}

impl Message {
    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub fn message_type(&self) -> String {
        self.message_type.clone()
    }

    pub fn content(&self) -> String {
        self.content.clone()
    }

    pub fn sender(&self) -> String {
        self.sender.clone()
    }

    pub fn origin_server_ts(&self) -> u64 {
        self.origin_server_ts.clone()
    }
}

pub fn sync_event_to_message(sync_event: SyncRoomEvent) -> Option<Arc<Message>> {
    match sync_event.event.deserialize() {
        Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomMessage(m))) => {
            let message = Message { 
                id: m.event_id.to_string(),
                message_type: m.content.msgtype().to_string(), 
                content: m.content.body().to_string(), 
                sender: m.sender.to_string(),
                origin_server_ts: m.origin_server_ts.as_secs().into()
            };

            Some(Arc::new(message))
        }
        _ => { None }
    }   
}