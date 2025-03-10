use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{spawn, task::JoinHandle, time::sleep};

const MESSAGE_DURATION: Duration = Duration::from_secs(4);

pub struct Status {
    /// Content of the latest status message, if set.
    pub last_status_message: Arc<Mutex<Option<String>>>,

    /// A task to automatically clear the status message in N seconds, if set.
    pub clear_status_message: Option<JoinHandle<()>>,
}

impl Status {
    /// Create a new empty [`Status`] widget.
    pub fn new() -> Self {
        Self { last_status_message: Default::default(), clear_status_message: Default::default() }
    }

    /// Set the current status message (displayed at the bottom), for a few
    /// seconds.
    pub fn set_message(&mut self, status: String) {
        if let Some(handle) = self.clear_status_message.take() {
            // Cancel the previous task to clear the status message.
            handle.abort();
        }

        *self.last_status_message.lock().unwrap() = Some(status);

        let message = self.last_status_message.clone();
        self.clear_status_message = Some(spawn(async move {
            // Clear the status message in 4 seconds.
            sleep(MESSAGE_DURATION).await;

            *message.lock().unwrap() = None;
        }));
    }
}
