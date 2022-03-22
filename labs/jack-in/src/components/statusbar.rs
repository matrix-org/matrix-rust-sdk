
use super::{Msg, get_block};

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::tui::{layout::Rect, widgets::{Paragraph, Tabs}, text::{Span, Spans} };
use tuirealm::{
    AttrValue, Attribute, Component, Event, Frame, MockComponent, NoUserEvent, Props, State,
};
use tuirealm::props::{Alignment, Borders, Color, Style, TextModifiers};
use super::super::client::state::SlidingSyncState;

/// ## StatusBar
///
/// Simple label component; just renders a text
/// NOTE: since I need just one label, I'm not going to use different object; I will directly implement Component for StatusBar.
/// This is not ideal actually and in a real app you should differentiate Mock Components from Application Components.
pub struct StatusBar {
    props: Props,
    sstate: SlidingSyncState
}

impl StatusBar {
    pub fn new(sstate: SlidingSyncState) -> Self {
        Self {
            props: Props::default(),
            sstate,
        }
    }
}

impl MockComponent for StatusBar {
    fn view(&mut self, frame: &mut Frame, area: Rect) {

        let title = ("Status".to_owned(), Alignment::Left);

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();

        let focus = self
            .props
            .get_or(Attribute::Focus, AttrValue::Flag(false))
            .unwrap_flag();


        let focus = self
            .props
            .get_or(Attribute::Focus, AttrValue::Flag(false))
            .unwrap_flag();

        let tabs = {
            let mut tabs = vec![];
            if let Some(dur) = self.sstate.time_to_first_render() {
                tabs.push(Spans::from(format!("First view: {}s", dur.as_secs())));

                if let Some(dur) = self.sstate.time_to_full_sync() {
                    tabs.push(Spans::from(format!("Full sync: {}s", dur.as_secs())));
                    if let Some(count) = self.sstate.total_rooms_count() {
                        tabs.push(Spans::from(format!("{} rooms", count)));
                    }
                } else {
                    tabs.push(Spans::from(format!("Loaded {:} rooms in {}s", self.sstate.loaded_rooms_count(), self.sstate.started().elapsed().as_secs())));
                }

            } else {
                tabs.push(Spans::from(format!("loading for {}s", self.sstate.started().elapsed().as_secs())));
            }
            tabs
        };

        frame.render_widget(
            Tabs::new(tabs)
                .style(Style::default().fg(Color::LightCyan))
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

impl Component<Msg, NoUserEvent> for StatusBar {
    fn on(&mut self, _: Event<NoUserEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
