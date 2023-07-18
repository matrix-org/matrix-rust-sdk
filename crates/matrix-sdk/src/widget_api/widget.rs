use tokio::sync::mpsc::Receiver;

use super::messages::Incoming;

#[allow(missing_debug_implementations)]
pub struct Widget {
    pub id: String,
    pub incoming: Receiver<Incoming>,
}
