use std::{
    sync::{
        mpsc::{self, Receiver},
        Arc,
    },
    time::Duration,
};

use matrix_sdk_common::locks::Mutex;
use ratatui::{
    prelude::{Buffer, Rect, *},
    widgets::Paragraph,
};
use tokio::{
    spawn,
    task::{spawn_blocking, JoinHandle},
    time::sleep,
};

use crate::{DetailsMode, GlobalMode};

const MESSAGE_DURATION: Duration = Duration::from_secs(4);

pub struct Status {
    /// Content of the latest status message, if set.
    last_status_message: Arc<Mutex<Option<String>>>,

    message_sender: mpsc::Sender<String>,

    _receiver_task: JoinHandle<()>,

    mode: DetailsMode,
    global_mode: GlobalMode,
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

        Self {
            last_status_message,
            _receiver_task: receiver_task,
            message_sender,
            mode: DetailsMode::default(),
            global_mode: GlobalMode::default(),
        }
    }

    fn receiving_task(receiver: Receiver<String>, status_message: Arc<Mutex<Option<String>>>) {
        let mut clear_message_task: Option<JoinHandle<()>> = None;

        while let Ok(message) = receiver.recv() {
            if let Some(task) = clear_message_task.take() {
                task.abort();
            }

            {
                let mut status_message = status_message.lock();
                *status_message = Some(message);
            }

            clear_message_task = Some(spawn({
                let status_message = status_message.clone();

                async move {
                    // Clear the status message in 4 seconds.
                    sleep(MESSAGE_DURATION).await;
                    status_message.lock().take();
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

    pub fn set_mode(&mut self, mode: DetailsMode) {
        self.mode = mode;
    }

    pub fn set_global_mode(&mut self, mode: GlobalMode) {
        self.global_mode = mode;
    }

    pub fn handle(&self) -> StatusHandle {
        StatusHandle { message_sender: self.message_sender.clone() }
    }
}

impl Widget for &mut Status {
    /// Render the bottom part of the screen, with a status message if one is
    /// set, or a default help message otherwise.
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let status_message = self.last_status_message.lock();

        let content = if let Some(status_message) = status_message.as_deref() {
            status_message
        } else {
            match self.global_mode {
                GlobalMode::Help => "Press q to exit the help screen",
                GlobalMode::Default => match self.mode {
                    DetailsMode::ReadReceipts => {
                        "\nUse j/k to move, s/S to start/stop the sync service, \
                     m to mark as read, t to show the timeline, e to show events."
                    }
                    DetailsMode::TimelineItems => {
                        "\nUse j/k to move, s/S to start/stop the sync service, \
                     r to show read receipts, e to show events, Q to enable/disable \
                     the send queue, M to send a message, L to like the last message."
                    }
                    DetailsMode::Events => {
                        "\nUse j/k to move, s/S to start/stop the sync service, r to show \
                     read receipts, t to show the timeline"
                    }
                    DetailsMode::LinkedChunk => {
                        "\nUse j/k to move, s/S to start/stop the sync service, r to show \
                     read receipts, t to show the timeline, e to show events"
                    }
                },
            }
        };

        Paragraph::new(content).centered().render(area, buf);
    }
}
