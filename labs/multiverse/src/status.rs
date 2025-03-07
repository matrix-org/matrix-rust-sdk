use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

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
}
