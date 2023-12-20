use tokio::task::JoinHandle;
use tracing::debug;

/// A task handle is a way to keep the handle a task running by itself in
/// detached mode.
///
/// It's a thin wrapper around [`JoinHandle`].
#[derive(uniffi::Object)]
pub struct TaskHandle {
    handle: JoinHandle<()>,
}

impl TaskHandle {
    // Create a new task handle.
    pub fn new(handle: JoinHandle<()>) -> Self {
        Self { handle }
    }
}

#[uniffi::export]
impl TaskHandle {
    // Cancel a task handle.
    pub fn cancel(&self) {
        debug!("Cancelling the task handle");

        self.handle.abort();
    }

    /// Check whether the handle is finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}
