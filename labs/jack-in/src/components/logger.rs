
use super::{Msg, get_block};

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::tui::{layout::Rect, widgets::Paragraph};
use tuirealm::{
    AttrValue, Attribute, Component, Event, Frame, MockComponent, NoUserEvent, Props, State,
};
use tuirealm::props::{Alignment, Borders, Color, Style, TextModifiers};
use tui_logger::TuiLoggerWidget;

/// ## Logger
///
/// Simple label component; just renders a text
/// NOTE: since I need just one label, I'm not going to use different object; I will directly implement Component for Logger.
/// This is not ideal actually and in a real app you should differentiate Mock Components from Application Components.
pub struct Logger {
    props: Props,
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            props: Props::default(),
        }
    }
}

impl MockComponent for Logger {
    fn view(&mut self, frame: &mut Frame, area: Rect) {

    let title = ("Logs".to_owned(), Alignment::Center);

    let borders = self
        .props
        .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
        .unwrap_borders();
    let focus = self
        .props
        .get_or(Attribute::Focus, AttrValue::Flag(false))
        .unwrap_flag();
        frame.render_widget(
            TuiLoggerWidget::default()
                .style_error(Style::default().fg(Color::Red))
                .style_debug(Style::default().fg(Color::Green))
                .style_warn(Style::default().fg(Color::Yellow))
                .style_trace(Style::default().fg(Color::Gray))
                .style_info(Style::default().fg(Color::Blue))
                .block(get_block(borders, title, focus))
                .style(Style::default().fg(Color::White).bg(Color::Black)),
            area
        );
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

impl Component<Msg, NoUserEvent> for Logger {
    fn on(&mut self, _: Event<NoUserEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
