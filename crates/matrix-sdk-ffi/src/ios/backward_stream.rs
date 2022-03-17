use super::messages::{sync_event_to_message, AnyMessage};
use super::RUNTIME;

use core::pin::Pin;
use futures::{pin_mut, StreamExt};
use matrix_sdk::deserialized_responses::SyncRoomEvent;
use parking_lot::Mutex;
use std::sync::Arc;

type MsgStream = Pin<Box<dyn futures::Stream<Item = Result<SyncRoomEvent, matrix_sdk::Error>>>>;

pub struct BackwardsStream {
    stream: Arc<Mutex<MsgStream>>,
}

unsafe impl Send for BackwardsStream {}
unsafe impl Sync for BackwardsStream {}

impl BackwardsStream {
    pub fn new(stream: MsgStream) -> Self {
        BackwardsStream { stream: Arc::new(Mutex::new(Box::pin(stream))) }
    }
    pub fn paginate_backwards(&self, mut count: u64) -> Vec<Arc<AnyMessage>> {
        let stream = self.stream.clone();
        RUNTIME.block_on(async move {
            let stream = stream.lock();
            pin_mut!(stream);
            let mut messages: Vec<Arc<AnyMessage>> = Vec::new();

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
