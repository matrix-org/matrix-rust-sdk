use super::{RUNTIME};
use super::messages::{Message, sync_event_to_message};

use core::pin::Pin;
use std::sync::Arc;
use futures::{pin_mut, StreamExt};
use matrix_sdk::deserialized_responses::SyncRoomEvent;
use parking_lot::{Mutex};

type MsgStream = Pin<Box<dyn futures::Stream<Item = Result<SyncRoomEvent, matrix_sdk::Error>>>>;

pub struct BackwardsStream {
    stream: Arc<Mutex<MsgStream>>,
}

unsafe impl Send for BackwardsStream { }
unsafe impl Sync for BackwardsStream { }

impl BackwardsStream {

    pub fn new(stream: MsgStream) -> Self {
        BackwardsStream {
            stream: Arc::new(Mutex::new(Box::pin(stream)))
        }
    }
    pub fn paginate_backwards(&self, mut count: u64) -> Vec<Arc<Message>> {
        let stream = self.stream.clone();
        RUNTIME.block_on(async move {
            let stream = stream.lock();
            pin_mut!(stream);
            let mut messages: Vec<Arc<Message>> = Vec::new();
            
            while count > 0 {
                match stream.next().await {
                    Some(Ok(e)) => {
                        if let Some(inner) = sync_event_to_message(e) {
                            messages.push(inner);
                            count -= 1;
                        }
                    }
                    None => {
                        // end of stream
                        break;
                    }
                    _ => {
                        // error cases, skipping
                    }
                }
            }

            messages
        })
    }
}