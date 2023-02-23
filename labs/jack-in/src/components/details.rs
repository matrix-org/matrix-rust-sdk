use std::collections::BTreeMap;

use chrono::{offset::Local, DateTime};
use matrix_sdk::room::timeline::TimelineItemContent;
use tuirealm::{
    command::{Cmd, CmdResult},
    event::{Key, KeyEvent, KeyModifiers},
    props::{Alignment, Borders, Color, Style},
    tui::{
        layout::{Constraint, Direction, Layout, Rect},
        style::Modifier,
        text::Spans,
        widgets::{Cell, List, ListItem, ListState, Row, Table, Tabs},
    },
    AttrValue, Attribute, Component, Event, Frame, MockComponent, Props, State,
};

use super::{super::client::state::SlidingSyncState, get_block, JackInEvent, Msg};

/// ## Details
pub struct Details {
    props: Props,
    sstate: SlidingSyncState,
    liststate: ListState,
    name: Option<String>,
    state_events_counts: Vec<(String, usize)>,
    current_room_timeline: Vec<String>,
}

impl Details {
    pub fn new(sstate: SlidingSyncState) -> Self {
        Self {
            props: Props::default(),
            sstate,
            liststate: Default::default(),
            name: None,
            state_events_counts: Default::default(),
            current_room_timeline: Default::default(),
        }
    }

    pub fn set_sliding_sync(&mut self, sstate: SlidingSyncState) {
        self.sstate = sstate;
        // we gotta refresh data next time it comes around
        self.name = None;
    }

    pub fn refresh_data(&mut self) {
        let Some(room_id) = self.sstate.selected_room.read().unwrap().clone() else { return };
        let Some(room_data) = self.sstate.get_room(&room_id) else {
            return;
        };

        let name = room_data.name().unwrap_or("unknown").to_owned();

        let state_events = room_data
            .required_state()
            .iter()
            .filter_map(|r| r.deserialize().ok())
            .fold(BTreeMap::<String, Vec<_>>::new(), |mut b, r| {
                let event_name = r.event_type();
                b.entry(event_name.to_string())
                    .and_modify(|l| l.push(r.clone()))
                    .or_insert_with(|| vec![r.clone()]);
                b
            });

        let mut state_events_counts: Vec<(String, usize)> =
            state_events.iter().map(|(k, l)| (k.clone(), l.len())).collect();
        state_events_counts.sort_by_key(|(_, count)| *count);

        let timeline: Vec<String> = self
            .sstate
            .current_timeline
            .read()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_event()) // we ignore virtual events
            .map(|e| match e.content() {
                TimelineItemContent::Message(m) => format!(
                    "[{}] {}: {}",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                    m.body()
                ),
                TimelineItemContent::RedactedMessage => format!(
                    "[{}] {} - redacted -",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                ),
                TimelineItemContent::Sticker(s) => format!(
                    "[{}] {}: {}",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                    s.content().body,
                ),
                TimelineItemContent::MembershipChange(m) => format!(
                    "[{}] {} - membership change '{:?}' for {}",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                    m.change(),
                    m.user_id(),
                ),
                TimelineItemContent::ProfileChange(_) => format!(
                    "[{}] {} - profile change",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                ),
                TimelineItemContent::OtherState(s) => format!(
                    "[{}] {}: '{}' state event",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                    s.content().event_type(),
                ),
                TimelineItemContent::UnableToDecrypt(_) => format!(
                    "[{}] {} - unable to decrypt -",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                ),
                TimelineItemContent::FailedToParseState { event_type, state_key, error } => {
                    format!(
                        "[{}] {} - failed to parse {event_type}({state_key}): {error}",
                        e.timestamp()
                            .to_system_time()
                            .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                            .unwrap_or_default(),
                        e.sender(),
                    )
                }
                TimelineItemContent::FailedToParseMessageLike { event_type, error } => format!(
                    "[{}] {} - failed to parse {event_type}: {error}",
                    e.timestamp()
                        .to_system_time()
                        .map(|s| DateTime::<Local>::from(s).format("%Y-%m-%dT%T").to_string())
                        .unwrap_or_default(),
                    e.sender(),
                ),
            })
            .collect();
        self.current_room_timeline = timeline;
        self.name = Some(name);
        self.state_events_counts = state_events_counts;
    }

    pub fn select_dir(&mut self, count: i32) {
        let total = self.current_room_timeline.len() as i32;
        let current = self.liststate.selected().unwrap_or_default() as i32;
        let next = {
            let next = current + count;
            if next >= total {
                next - total
            } else if next < 0 {
                total + next
            } else {
                next
            }
        };
        self.liststate.select(Some(next.try_into().unwrap_or_default()));
    }

    pub fn borders(mut self, b: Borders) -> Self {
        self.attr(Attribute::Borders, AttrValue::Borders(b));
        self
    }
}

impl MockComponent for Details {
    fn view(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.name.is_none() {
            self.refresh_data();
        }

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();

        let Some(name) = &self.name else {
            // still empty
            frame.render_widget(
                Table::new(vec![Row::new(vec![Cell::from(
                    "Choose a room with up/down and press <enter> to select",
                )])])
                .block(get_block(
                    borders,
                    ("".to_owned(), Alignment::Left),
                    false,
                )),
                area,
            );
            return;
        };

        let areas = vec![
            Constraint::Length(3), // Events
            Constraint::Min(10),   // Timeline
        ];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(areas)
            .split(area);

        let events_title = ("Events".to_owned(), Alignment::Left);

        let events_borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();

        let mut tabs = vec![];

        for (title, count) in &self.state_events_counts {
            tabs.push(Spans::from(format!("{title}: {count}")));
        }

        frame.render_widget(
            Tabs::new(tabs)
                .style(Style::default().fg(Color::LightCyan))
                .block(get_block(events_borders, events_title, false))
                .style(Style::default().fg(Color::White).bg(Color::Black)),
            chunks[0],
        );

        let title = (name.to_owned(), Alignment::Left);
        let focus = self.props.get_or(Attribute::Focus, AttrValue::Flag(false)).unwrap_flag();

        let details: Vec<_> =
            self.current_room_timeline.iter().map(|e| ListItem::new(e.clone())).collect();

        frame.render_stateful_widget(
            List::new(details)
                .style(Style::default().fg(Color::White))
                .highlight_style(
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::ITALIC),
                )
                .highlight_symbol(">>")
                .block(get_block(borders, title, focus)),
            chunks[1],
            &mut self.liststate,
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

impl Component<Msg, JackInEvent> for Details {
    fn on(&mut self, ev: Event<JackInEvent>) -> Option<Msg> {
        let focus = self.props.get_or(Attribute::Focus, AttrValue::Flag(false)).unwrap_flag();
        if focus {
            // we only care about user input if we are in focus.
            match ev {
                Event::Keyboard(KeyEvent { code: Key::Down, modifiers: KeyModifiers::NONE }) => {
                    self.select_dir(1);
                    return None;
                }
                Event::Keyboard(KeyEvent { code: Key::Down, modifiers: KeyModifiers::SHIFT }) => {
                    self.select_dir(10);
                    return None;
                }
                Event::Keyboard(KeyEvent { code: Key::Up, modifiers: KeyModifiers::NONE }) => {
                    self.select_dir(-1);
                    return None;
                }
                Event::Keyboard(KeyEvent { code: Key::Up, modifiers: KeyModifiers::SHIFT }) => {
                    self.select_dir(-10);
                    return None;
                }
                Event::Keyboard(KeyEvent { code: Key::Tab, modifiers: KeyModifiers::NONE }) => {
                    return Some(Msg::DetailsBlur)
                } // Return focus lost
                Event::Keyboard(KeyEvent { code: Key::Esc, modifiers: KeyModifiers::NONE }) => {
                    return Some(Msg::AppClose)
                }
                _ => {}
            }
        }

        if let Event::User(JackInEvent::SyncUpdate(s)) = ev {
            self.set_sliding_sync(s);
        }

        None
    }
}
