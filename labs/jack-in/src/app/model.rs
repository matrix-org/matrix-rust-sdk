//! ## Model
//!
//! app model

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::sync::RwLock;
use tuirealm::{
    props::{Alignment, Borders, Color, TextModifiers},
    terminal::TerminalBridge,
    tui::layout::{Constraint, Direction, Layout},
    Application, AttrValue, Attribute, EventListenerCfg, Sub, SubClause, SubEventClause, Update,
};

/**
 * MIT License
 *
 * tui-realm - Copyright (C) 2021 Christian Visintin
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the
 * "Software"), to deal in the Software without restriction, including
 * without limitation the rights to use, copy, modify, merge, publish,
 * distribute, sublicense, and/or sell copies of the Software, and to permit
 * persons to whom the Software is furnished to do so, subject to the
 * following conditions:
 *
 * The above copyright notice and this permission notice shall be included
 * in all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
 * OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
 * NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
 * DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
 * OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
 * USE OR OTHER DEALINGS IN THE SOFTWARE.
 */
use super::components::{Details, Label, Logger, Rooms, StatusBar};
use super::{Id, JackInEvent, MatrixPoller, Msg};
use crate::client::state::SlidingSyncState;

pub struct Model {
    /// Application
    pub app: Application<Id, Msg, JackInEvent>,
    pub title: String,
    /// Indicates that the application must quit
    pub quit: bool,
    /// Tells whether to redraw interface
    pub redraw: bool,
    /// Used to draw to terminal
    pub terminal: TerminalBridge,
    /// show the logger console
    pub show_logger: bool,
    sliding_sync: SlidingSyncState,
}

impl Model {
    pub(crate) fn new(sliding_sync: SlidingSyncState, poller: MatrixPoller) -> Self {
        let app = Self::init_app(sliding_sync.clone(), poller);

        Self {
            app,
            title: "Loading".to_owned(),
            sliding_sync,
            quit: false,
            redraw: true,
            terminal: TerminalBridge::new().expect("Cannot initialize terminal"),
            show_logger: true,
        }
    }
}

impl Model {
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

                self.app.view(&Id::Rooms, f, body_chunks[0]);
                self.app.view(&Id::Details, f, body_chunks[1]);
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
        // Setup application
        // NOTE: JackInEvent is a shorthand to tell tui-realm we're not going to use any
        // custom user event NOTE: the event listener is configured to use the
        // default crossterm input listener and to raise a Tick event each second
        // which we will use to update the clock

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
                        .alignment(Alignment::Left)
                        .background(Color::Reset)
                        .foreground(Color::LightYellow)
                        .modifiers(TextModifiers::BOLD),
                ),
                Vec::default(),
            )
            .is_ok());
        /// mount logger
        assert!(app.remount(Id::Logger, Box::new(Logger::default()), Vec::default()).is_ok());

        assert!(app
            .mount(
                Id::Status,
                Box::new(StatusBar::new(sliding_sync.clone())),
                vec![Sub::new(SubEventClause::Any, SubClause::Always)]
            )
            .is_ok());

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
                    // Give focus to letter counter
                    let _ = self.app.blur();
                    assert!(self.app.active(&Id::Details).is_ok());
                    None
                }
                Msg::DetailsBlur => {
                    // Give focus to letter counter
                    let _ = self.app.blur();
                    assert!(self.app.active(&Id::Rooms).is_ok());
                    None
                }
                Msg::SelectRoom(r) => {
                    self.sliding_sync.select_room(r);
                    None
                }
                _ => None,
            }
        } else {
            None
        }
    }
}
