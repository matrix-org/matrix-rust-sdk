//! ## Model
//!
//! app model

/**
 * MIT License
 *
 * tui-realm - Copyright (C) 2021 Christian Visintin
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
use super::components::{Clock, DigitCounter, Label, Logger, StatusBar, Rooms, LetterCounter};
use super::{Id, Msg};

use std::time::{Duration, SystemTime};
use tuirealm::props::{Alignment, Color, TextModifiers};
use tuirealm::terminal::TerminalBridge;
use tuirealm::tui::layout::{Constraint, Direction, Layout};
use tuirealm::{
    event::NoUserEvent, Application, AttrValue, Attribute, EventListenerCfg, Sub, SubClause,
    SubEventClause, Update,
};

use crate::client::state::SlidingSyncState;
use tokio::sync::RwLock;
use std::sync::Arc;

pub struct Model {
    /// Application
    pub app: Application<Id, Msg, NoUserEvent>,
    /// Indicates that the application must quit
    pub quit: bool,
    /// Tells whether to redraw interface
    pub redraw: bool,
    /// Used to draw to terminal
    pub terminal: TerminalBridge,
    /// State for SlidingSync.
    pub sliding_sync: SlidingSyncState,
    /// show the logger console
    pub show_logger: bool,
}

impl Model {
    pub fn new(sliding_sync: SlidingSyncState) -> Self {
        Self {
            app: Self::init_app(sliding_sync.clone()),
            quit: false,
            redraw: true,
            terminal: TerminalBridge::new().expect("Cannot initialize terminal"),
            sliding_sync,
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
                    Constraint::Length(3), // Clock
                    Constraint::Min(10),     // body
                    Constraint::Length(3), // Status Footer
                    Constraint::Length(1), // Label
                ];
                if self.show_logger {
                    areas.push(
                        Constraint::Length(12),  // logs 
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

                self.app.view(&Id::Clock, f, chunks[0]);
                self.app.view(&Id::Rooms, f, body_chunks[0]);
                self.app.view(&Id::Status, f, chunks[2]);
                self.app.view(&Id::Label, f, chunks[3]);
                if self.show_logger {
                    self.app.view(&Id::Logger, f, chunks[4]);
                }
            })
            .is_ok());
    }

    fn mount_sliding_sync_components(&mut self) {

        assert!(self.app
            .remount(
                Id::Status,
                Box::new(StatusBar::new(self.sliding_sync.clone())),
                Vec::default()
            )
            .is_ok());
    
        assert!(self.app
            .remount(
                Id::Rooms,
                Box::new(Rooms::new(self.sliding_sync.clone())),
                Vec::default()
            )
            .is_ok());
    }

    fn init_app(sliding_sync: SlidingSyncState) -> Application<Id, Msg, NoUserEvent> {
        // Setup application
        // NOTE: NoUserEvent is a shorthand to tell tui-realm we're not going to use any custom user event
        // NOTE: the event listener is configured to use the default crossterm input listener and to raise a Tick event each second
        // which we will use to update the clock

        let mut app: Application<Id, Msg, NoUserEvent> = Application::init(
            EventListenerCfg::default()
                .default_input_listener(Duration::from_millis(20))
                .poll_timeout(Duration::from_millis(10))
                .tick_interval(Duration::from_secs(1)),
        );
        // Mount components
        assert!(app
            .mount(
                Id::Label,
                Box::new(
                    Label::default()
                        .text("Waiting for a Msg...")
                        .alignment(Alignment::Left)
                        .background(Color::Reset)
                        .foreground(Color::LightYellow)
                        .modifiers(TextModifiers::BOLD),
                ),
                Vec::default(),
            )
            .is_ok());
        // Mount clock, subscribe to tick
        assert!(app
            .mount(
                Id::Clock,
                Box::new(
                    Clock::new(SystemTime::now())
                        .alignment(Alignment::Center)
                        .background(Color::Reset)
                        .foreground(Color::Cyan)
                        .modifiers(TextModifiers::BOLD)
                ),
                vec![Sub::new(SubEventClause::Tick, SubClause::Always)]
            )
            .is_ok());
        // Mount counters
        assert!(app
            .mount(
                Id::LetterCounter,
                Box::new(LetterCounter::new(0)),
                Vec::new()
            )
            .is_ok());
        assert!(app
            .mount(
                Id::DigitCounter,
                Box::new(DigitCounter::new(5)),
                Vec::default()
            )
            .is_ok());
        /// mount logger
        assert!(app
            .remount(
                Id::Logger,
                Box::new(Logger::default()),
                Vec::default()
            )
            .is_ok());
        
        assert!(app
            .mount(
                Id::Status,
                Box::new(StatusBar::new(sliding_sync.clone())),
                Vec::default()
            )
        .is_ok());
    
        assert!(app
            .mount(
                Id::Rooms,
                Box::new(Rooms::new(sliding_sync)),
                Vec::default()
            )
        .is_ok());
        //app.mount_sliding_sync_components();
        // Active letter counter
        assert!(app.active(&Id::LetterCounter).is_ok());
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
                Msg::SyncUpdate(s) => {
                    self.sliding_sync = s;
                    self.mount_sliding_sync_components();
                    None
                }
                Msg::DigitCounterBlur => {
                    // Give focus to letter counter
                    assert!(self.app.active(&Id::LetterCounter).is_ok());
                    None
                }
                Msg::DigitCounterChanged(v) => {
                    // Update label
                    assert!(self
                        .app
                        .attr(
                            &Id::Label,
                            Attribute::Text,
                            AttrValue::String(format!("DigitCounter has now value: {}", v))
                        )
                        .is_ok());
                    None
                }
                Msg::LetterCounterBlur => {
                    // Give focus to digit counter
                    assert!(self.app.active(&Id::DigitCounter).is_ok());
                    None
                }
                Msg::LetterCounterChanged(v) => {
                    // Update label
                    assert!(self
                        .app
                        .attr(
                            &Id::Label,
                            Attribute::Text,
                            AttrValue::String(format!("LetterCounter has now value: {}", v))
                        )
                        .is_ok());
                    None
                }
            }
        } else {
            None
        }
    }
}
