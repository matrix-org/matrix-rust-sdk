use tui_logger::TuiLoggerWidget;
use tuirealm::{
    command::{Cmd, CmdResult},
    props::{Alignment, Borders, Color, Style},
    tui::layout::Rect,
    AttrValue, Attribute, Component, Event, Frame, MockComponent, Props, State,
};

use super::{get_block, JackInEvent, Msg};

/// ## Logger
///
/// Simple label component; just renders a text
/// NOTE: since I need just one label, I'm not going to use different object; I
/// will directly implement Component for Logger. This is not ideal actually and
/// in a real app you should differentiate Mock Components from Application
/// Components.
#[derive(Default)]
pub struct Logger {
    props: Props,
}

impl MockComponent for Logger {
    fn view(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = ("Logs".to_owned(), Alignment::Center);

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();
        let focus = self.props.get_or(Attribute::Focus, AttrValue::Flag(false)).unwrap_flag();
        frame.render_widget(
            TuiLoggerWidget::default()
                .style_error(Style::default().fg(Color::Red))
                .style_debug(Style::default().fg(Color::Green))
                .style_warn(Style::default().fg(Color::Yellow))
                .style_trace(Style::default().fg(Color::Gray))
                .style_info(Style::default().fg(Color::Blue))
                .block(get_block(borders, title, focus))
                .style(Style::default().fg(Color::White).bg(Color::Black)),
            area,
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

impl Component<Msg, JackInEvent> for Logger {
    fn on(&mut self, _: Event<JackInEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
