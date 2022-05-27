use core::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use matrix_sdk::{deserialized_responses::SyncRoomEvent, locks::Mutex, Result};
use tokio_stream::StreamExt;
use tracing::error;

use super::{
    messages::{sync_event_to_message, AnyMessage},
    RUNTIME,
};

type MsgStream = Pin<Box<dyn Stream<Item = Result<SyncRoomEvent>> + Send>>;

pub struct BackwardsStream {
    stream: Arc<Mutex<MsgStream>>,
}

impl BackwardsStream {
    pub fn new(stream: MsgStream) -> Self {
        BackwardsStream { stream: Arc::new(Mutex::new(Box::pin(stream))) }
    }

    pub fn paginate_backwards(&self, count: u64) -> Vec<Arc<AnyMessage>> {
        let stream = self.stream.clone();
        RUNTIME.block_on(async move {
            let mut stream = stream.lock().await;
            (&mut *stream)
                .take(count as usize)
                .filter_map(|r| match r {
                    Ok(ev) => sync_event_to_message(ev),
                    Err(e) => {
                        error!("Pagniation error: {e}");
                        None
                    }
                })
                .collect()
                .await
        })
    }
}
