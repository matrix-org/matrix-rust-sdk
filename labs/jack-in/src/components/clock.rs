//! ## Label
//!
//! label component

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
use super::{Label, Msg, JackInEvent};

use std::ops::Add;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::props::{Alignment, Color, TextModifiers};
use tuirealm::tui::layout::Rect;
use tuirealm::{
    AttrValue, Attribute, Component, Event, Frame, MockComponent, State, StateValue,
};

/// ## Clock
///
/// Simple clock component which displays current time
pub struct Clock {
    component: Label,
    states: OwnStates,
}

impl Clock {
    pub fn new(initial_time: SystemTime) -> Self {
        Self {
            component: Label::default(),
            states: OwnStates::new(initial_time),
        }
    }

    pub fn alignment(mut self, a: Alignment) -> Self {
        self.component
            .attr(Attribute::TextAlign, AttrValue::Alignment(a));
        self
    }

    pub fn foreground(mut self, c: Color) -> Self {
        self.component
            .attr(Attribute::Foreground, AttrValue::Color(c));
        self
    }

    pub fn background(mut self, c: Color) -> Self {
        self.component
            .attr(Attribute::Background, AttrValue::Color(c));
        self
    }

    pub fn modifiers(mut self, m: TextModifiers) -> Self {
        self.attr(Attribute::TextProps, AttrValue::TextModifiers(m));
        self
    }

    fn time_to_str(&self) -> String {
        let since_the_epoch = self.get_epoch_time();
        let hours = (since_the_epoch / 3600) % 24;
        let minutes = (since_the_epoch / 60) % 60;
        let seconds = since_the_epoch % 60;
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }

    fn get_epoch_time(&self) -> u64 {
        self.states
            .time
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs()
    }
}

impl MockComponent for Clock {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // Render
        self.component.view(frame, area);
    }

    fn query(&self, attr: Attribute) -> Option<AttrValue> {
        self.component.query(attr)
    }

    fn attr(&mut self, attr: Attribute, value: AttrValue) {
        self.component.attr(attr, value);
    }

    fn state(&self) -> State {
        // Return current time
        State::One(StateValue::U64(self.get_epoch_time()))
    }

    fn perform(&mut self, cmd: Cmd) -> CmdResult {
        self.component.perform(cmd)
    }
}

impl Component<Msg, JackInEvent> for Clock {
    fn on(&mut self, ev: Event<JackInEvent>) -> Option<Msg> {
        if let Event::Tick = ev {
            self.states.tick();
            // Set text
            self.attr(Attribute::Text, AttrValue::String(self.time_to_str()));
            Some(Msg::Clock)
        } else {
            None
        }
    }
}

struct OwnStates {
    time: SystemTime,
}

impl OwnStates {
    pub fn new(time: SystemTime) -> Self {
        Self { time }
    }

    pub fn tick(&mut self) {
        self.time = self.time.add(Duration::from_secs(1));
    }
}
