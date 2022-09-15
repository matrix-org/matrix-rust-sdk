//! ## Label
//!
//! label component

use tuirealm::{
    command::{Cmd, CmdResult},
    props::{Alignment, Borders, Color, Style, TextModifiers},
    tui::{
        layout::Rect,
        widgets::{Block, Paragraph},
    },
    AttrValue, Attribute, Component, Event, Frame, MockComponent, Props, State,
};

use super::{JackInEvent, Msg};

/// ## Label
///
/// Simple label component; just renders a text
/// NOTE: since I need just one label, I'm not going to use different object; I
/// will directly implement Component for Label. This is not ideal actually and
/// in a real app you should differentiate Mock Components from Application
/// Components.
#[derive(Default)]
pub struct Label {
    props: Props,
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

    pub fn borders(mut self, b: Borders) -> Self {
        self.attr(Attribute::Borders, AttrValue::Borders(b));
        self
    }
}

impl MockComponent for Label {
    fn view(&mut self, frame: &mut Frame<'_>, area: Rect) {
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
                .get_or(Attribute::TextProps, AttrValue::TextModifiers(TextModifiers::empty()))
                .unwrap_text_modifiers();

            let borders = self
                .props
                .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
                .unwrap_borders();

            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(borders.sides).border_style(borders.style()))
                    .style(Style::default().fg(foreground).bg(background).add_modifier(modifiers))
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

impl Component<Msg, JackInEvent> for Label {
    fn on(&mut self, _: Event<JackInEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
