//! ## Model
//!
//! app model

use std::time::Duration;

use futures::executor::block_on;
use matrix_sdk::{ruma::events::room::message::RoomMessageEventContent, Client};
use tokio::sync::mpsc;
use tracing::warn;
use tuirealm::{
    props::{Alignment, Borders, Color},
    terminal::TerminalBridge,
    tui::layout::{Constraint, Direction, Layout},
    Application, AttrValue, Attribute, EventListenerCfg, Sub, SubClause, SubEventClause, Update,
};

use super::{
    components::{Details, InputText, Label, Logger, Rooms, StatusBar},
    Id, JackInEvent, MatrixPoller, Msg,
};
use crate::client::state::SlidingSyncState;

pub struct Model {
    /// Application
    pub app: Application<Id, Msg, JackInEvent>,
    /// Indicates that the application must quit
    pub quit: bool,
    /// Tells whether to redraw interface
    pub redraw: bool,
    /// Used to draw to terminal
    pub terminal: TerminalBridge,
    /// show the logger console
    pub show_logger: bool,
    sliding_sync: SlidingSyncState,
    tx: mpsc::Sender<SlidingSyncState>,
    pub client: Client,
}

impl Model {
    pub(crate) fn new(
        sliding_sync: SlidingSyncState,
        tx: mpsc::Sender<SlidingSyncState>,
        poller: MatrixPoller,
        client: Client,
    ) -> Self {
        let app = Self::init_app(sliding_sync.clone(), poller);

        Self {
            app,
            tx,
            sliding_sync,
            quit: false,
            redraw: true,
            terminal: TerminalBridge::new().expect("Cannot initialize terminal"),
            show_logger: true,
            client,
        }
    }
}

impl Model {
    pub fn set_title(&mut self, title: String) {
        assert!(self.app.attr(&Id::Label, Attribute::Text, AttrValue::String(title),).is_ok());
    }
    pub fn view(&mut self) {
        assert!(self
            .terminal
            .raw_mut()
            .draw(|f| {
                let mut areas = vec![
                    Constraint::Length(3), // Header
                    Constraint::Min(10),   // body
                    Constraint::Length(3), // Status Footer
                ];
                if self.show_logger {
                    areas.push(
                        Constraint::Length(12), // logs
                    );
                }
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints(areas)
                    .split(f.size());

                // Body
                let body_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(35), Constraint::Min(23)].as_ref())
                    .split(chunks[1]);

                let details_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(10), Constraint::Max(3)].as_ref())
                    .split(body_chunks[1]);

                self.app.view(&Id::Rooms, f, body_chunks[0]);
                self.app.view(&Id::Details, f, details_chunks[0]);
                self.app.view(&Id::TextMessage, f, details_chunks[1]);
                self.app.view(&Id::Status, f, chunks[2]);
                self.app.view(&Id::Label, f, chunks[0]);
                if self.show_logger {
                    self.app.view(&Id::Logger, f, chunks[3]);
                }
            })
            .is_ok());
    }

    fn init_app(
        sliding_sync: SlidingSyncState,
        poller: MatrixPoller,
    ) -> Application<Id, Msg, JackInEvent> {
        let mut app: Application<Id, Msg, JackInEvent> = Application::init(
            EventListenerCfg::default()
                .default_input_listener(Duration::from_millis(20))
                .port(Box::new(poller), Duration::from_millis(100))
                .poll_timeout(Duration::from_millis(10))
                .tick_interval(Duration::from_millis(200)),
        );
        // Mount components
        assert!(app
            .mount(
                Id::Label,
                Box::new(
                    Label::default()
                        .text("Loading")
                        .alignment(Alignment::Center)
                        .background(Color::Reset)
                        .borders(Borders::default())
                        .foreground(Color::Green),
                ),
                Vec::default(),
            )
            .is_ok());
        // mount logger
        assert!(app.remount(Id::Logger, Box::<Logger>::default(), Vec::default()).is_ok());

        assert!(app
            .mount(
                Id::Status,
                Box::new(StatusBar::new(sliding_sync.clone())),
                vec![Sub::new(SubEventClause::Any, SubClause::Always)]
            )
            .is_ok());

        assert!(app.mount(Id::TextMessage, Box::<InputText>::default(), vec![]).is_ok());
        assert!(app
            .mount(
                Id::Rooms,
                Box::new(
                    Rooms::new(sliding_sync.clone())
                        .borders(Borders::default().color(Color::Green))
                ),
                vec![Sub::new(SubEventClause::Any, SubClause::Always)]
            )
            .is_ok());

        assert!(app
            .mount(
                Id::Details,
                Box::new(
                    Details::new(sliding_sync).borders(Borders::default().color(Color::Green))
                ),
                vec![Sub::new(SubEventClause::Any, SubClause::Always)]
            )
            .is_ok());
        // Active letter counter
        assert!(app.active(&Id::Rooms).is_ok());
        app
    }
}

// Let's implement Update for model

impl Update<Msg> for Model {
    fn update(&mut self, msg: Option<Msg>) -> Option<Msg> {
        if let Some(msg) = msg {
            // Set redraw
            self.redraw = true;
            // Match message
            match msg {
                Msg::AppClose => {
                    self.quit = true; // Terminate
                    None
                }
                Msg::Clock => None,
                Msg::RoomsBlur => {
                    // Give focus to room details
                    if self.sliding_sync.has_selected_room() {
                        let _ = self.app.blur();
                        assert!(self.app.active(&Id::Details).is_ok());
                    }
                    None
                }
                Msg::DetailsBlur => {
                    // Give focus to room list
                    let _ = self.app.blur();
                    assert!(self.app.active(&Id::TextMessage).is_ok());
                    None
                }
                Msg::TextBlur => {
                    // Give focus to room list
                    let _ = self.app.blur();
                    assert!(self.app.active(&Id::Rooms).is_ok());
                    None
                }
                Msg::SelectRoom(r) => {
                    warn!("setting room, sending msg");
                    self.sliding_sync.select_room(r);
                    let _ = self.tx.try_send(self.sliding_sync.clone());
                    None
                }
                Msg::SendMessage(m) => {
                    if let Some(tl) = &*self.sliding_sync.room_timeline.read() {
                        block_on(async move {
                            // fire and forget
                            tl.send(RoomMessageEventContent::text_plain(m).into(), None).await;
                        });
                    } else {
                        warn!("asked to send message, but no room is selected");
                    }
                    None
                }
            }
        } else {
            None
        }
    }
}
