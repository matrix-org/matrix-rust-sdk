use std::{
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{
    spawn,
    task::{spawn_blocking, JoinHandle},
    time::sleep,
};

const MESSAGE_DURATION: Duration = Duration::from_secs(4);

pub struct Status {
    /// Content of the latest status message, if set.
    pub last_status_message: Arc<Mutex<Option<String>>>,

    message_sender: mpsc::Sender<String>,

    _receiver_task: JoinHandle<()>,
}

pub struct StatusHandle {
    message_sender: mpsc::Sender<String>,
}

impl StatusHandle {
    /// Set the current status message (displayed at the bottom), for a few
    /// seconds.
    pub fn set_message(&self, status: String) {
        self.message_sender.send(status).expect(
            "We should be able to send the status message since the receiver is alive \
                  as long as we are alive",
        );
    }
}

impl Status {
    /// Create a new empty [`Status`] widget.
    pub fn new() -> Self {
        let (message_sender, receiver) = mpsc::channel();
        let last_status_message = Arc::new(Mutex::new(None));

        let receiver_task = spawn_blocking({
            let last_status_message = last_status_message.clone();
            move || Self::receiving_task(receiver, last_status_message)
        });

        Self { last_status_message, _receiver_task: receiver_task, message_sender }
    }

    fn receiving_task(receiver: Receiver<String>, status_message: Arc<Mutex<Option<String>>>) {
        let mut clear_message_task: Option<JoinHandle<()>> = None;

        while let Ok(message) = receiver.recv() {
            if let Some(task) = clear_message_task.take() {
                task.abort();
            }

            {
                let mut status_message = status_message.lock().unwrap();
                *status_message = Some(message);
            }

            clear_message_task = Some(spawn({
                let status_message = status_message.clone();

                async move {
                    // Clear the status message in 4 seconds.
                    sleep(MESSAGE_DURATION).await;
                    status_message.lock().unwrap().take();
                }
            }));
        }
    }

    /// Set the current status message (displayed at the bottom), for a few
    /// seconds.
    pub fn set_message(&self, status: String) {
        self.message_sender.send(status).expect(
            "We should be able to send the status message since the receiver is alive \
                  as long as we are alive",
        );
    }

    pub fn handle(&self) -> StatusHandle {
        StatusHandle { message_sender: self.message_sender.clone() }
    }
}
