use tuirealm::{
    command::{Cmd, CmdResult},
    props::{Alignment, Borders, Color, Style},
    tui::{layout::Rect, text::Spans, widgets::Tabs},
    AttrValue, Attribute, Component, Event, Frame, MockComponent, Props, State,
};

use super::{super::client::state::SlidingSyncState, get_block, JackInEvent, Msg};

/// ## StatusBar
pub struct StatusBar {
    props: Props,
    sstate: SlidingSyncState,
}

impl StatusBar {
    pub fn new(sstate: SlidingSyncState) -> Self {
        Self { props: Props::default(), sstate }
    }
    pub fn set_sliding_sync(&mut self, sstate: SlidingSyncState) {
        self.sstate = sstate;
    }
}

impl MockComponent for StatusBar {
    fn view(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = ("Status".to_owned(), Alignment::Left);

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();

        let focus = self.props.get_or(Attribute::Focus, AttrValue::Flag(false)).unwrap_flag();

        let tabs = {
            let mut tabs = vec![];
            if let Some(dur) = self.sstate.time_to_first_render() {
                tabs.push(Spans::from(format!("First view: {}ms", dur.as_millis())));

                if let Some(dur) = self.sstate.time_to_full_sync() {
                    tabs.push(Spans::from(format!("Full sync: {}ms", dur.as_millis())));
                    if let Some(count) = self.sstate.total_rooms_count() {
                        tabs.push(Spans::from(format!("{count} rooms")));
                    }
                } else {
                    tabs.push(Spans::from(format!(
                        "Loaded {:} rooms in {}s",
                        self.sstate.loaded_rooms_count(),
                        self.sstate.started().elapsed().as_secs()
                    )));
                }
            } else {
                tabs.push(Spans::from(format!(
                    "loading for {}s",
                    self.sstate.started().elapsed().as_secs()
                )));
            }
            tabs
        };

        frame.render_widget(
            Tabs::new(tabs)
                .style(Style::default().fg(Color::LightCyan))
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

impl Component<Msg, JackInEvent> for StatusBar {
    fn on(&mut self, ev: Event<JackInEvent>) -> Option<Msg> {
        if let Event::User(JackInEvent::SyncUpdate(s)) = ev {
            self.set_sliding_sync(s);
            None
        } else if let Event::Tick = ev {
            Some(Msg::Clock)
        } else {
            None
        }
    }
}
