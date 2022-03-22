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
use super::Msg;

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::props::{Alignment, Color, Style, TextModifiers};
use tuirealm::tui::{layout::Rect, widgets::Paragraph};
use tuirealm::{
    AttrValue, Attribute, Component, Event, Frame, MockComponent, NoUserEvent, Props, State,
};

/// ## Label
///
/// Simple label component; just renders a text
/// NOTE: since I need just one label, I'm not going to use different object; I will directly implement Component for Label.
/// This is not ideal actually and in a real app you should differentiate Mock Components from Application Components.
pub struct Label {
    props: Props,
}

impl Default for Label {
    fn default() -> Self {
        Self {
            props: Props::default(),
        }
    }
}

impl Label {
    pub fn text<S>(mut self, s: S) -> Self
    where
        S: AsRef<str>,
    {
        self.attr(Attribute::Text, AttrValue::String(s.as_ref().to_string()));
        self
    }

    pub fn alignment(mut self, a: Alignment) -> Self {
        self.attr(Attribute::TextAlign, AttrValue::Alignment(a));
        self
    }

    pub fn foreground(mut self, c: Color) -> Self {
        self.attr(Attribute::Foreground, AttrValue::Color(c));
        self
    }

    pub fn background(mut self, c: Color) -> Self {
        self.attr(Attribute::Background, AttrValue::Color(c));
        self
    }

    pub fn modifiers(mut self, m: TextModifiers) -> Self {
        self.attr(Attribute::TextProps, AttrValue::TextModifiers(m));
        self
    }
}

impl MockComponent for Label {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // Check if visible
        if self.props.get_or(Attribute::Display, AttrValue::Flag(true)) == AttrValue::Flag(true) {
            // Get properties
            let text = self
                .props
                .get_or(Attribute::Text, AttrValue::String(String::default()))
                .unwrap_string();
            let alignment = self
                .props
                .get_or(Attribute::TextAlign, AttrValue::Alignment(Alignment::Left))
                .unwrap_alignment();
            let foreground = self
                .props
                .get_or(Attribute::Foreground, AttrValue::Color(Color::Reset))
                .unwrap_color();
            let background = self
                .props
                .get_or(Attribute::Background, AttrValue::Color(Color::Reset))
                .unwrap_color();
            let modifiers = self
                .props
                .get_or(
                    Attribute::TextProps,
                    AttrValue::TextModifiers(TextModifiers::empty()),
                )
                .unwrap_text_modifiers();
            frame.render_widget(
                Paragraph::new(text)
                    .style(
                        Style::default()
                            .fg(foreground)
                            .bg(background)
                            .add_modifier(modifiers),
                    )
                    .alignment(alignment),
                area,
            );
        }
    }

    fn query(&self, attr: Attribute) -> Option<AttrValue> {
        self.props.get(attr)
    }

    fn attr(&mut self, attr: Attribute, value: AttrValue) {
        self.props.set(attr, value);
    }

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::None
    }
}

impl Component<Msg, NoUserEvent> for Label {
    fn on(&mut self, _: Event<NoUserEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
