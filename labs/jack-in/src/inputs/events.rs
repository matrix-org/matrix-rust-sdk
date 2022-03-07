use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::error;

use super::key::Key;
use super::InputEvent;

/// A small event handler that wrap crossterm input and tick event. Each event
/// type is handled in its own thread and returned to a common `Receiver`
pub struct Events {
    rx: tokio::sync::mpsc::Receiver<InputEvent>,
    // Need to be kept around to prevent disposing the sender side.
    _tx: tokio::sync::mpsc::Sender<InputEvent>,
    // To stop the loop
    stop_capture: Arc<AtomicBool>,
}

impl Events {
    /// Constructs an new instance of `Events` with the default config.
    pub fn new(tick_rate: Duration) -> Events {
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let stop_capture = Arc::new(AtomicBool::new(false));

        let event_tx = tx.clone();
        let event_stop_capture = stop_capture.clone();
        tokio::spawn(async move {
            loop {
                // poll for tick rate duration, if no event, sent tick event.
                if crossterm::event::poll(tick_rate).unwrap() {
                    if let crossterm::event::Event::Key(key) = crossterm::event::read().unwrap() {
                        let key = Key::from(key);
                        if let Err(err) = event_tx.send(InputEvent::Input(key)).await {
                            error!("Oops!, {}", err);
                        }
                    }
                }
                if let Err(err) = event_tx.send(InputEvent::Tick).await {
                    error!("Oops!, {}", err);
                }
                if event_stop_capture.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        Events {
            rx,
            _tx: tx,
            stop_capture,
        }
    }

    /// Attempts to read an event.
    pub async fn next(&mut self) -> InputEvent {
        self.rx.recv().await.unwrap_or(InputEvent::Tick)
    }

    /// Close
    pub fn close(&mut self) {
        self.stop_capture.store(true, Ordering::Relaxed)
    }
}
