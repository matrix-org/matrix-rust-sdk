use tracing::warn;
use tuirealm::{
    command::{Cmd, CmdResult},
    event::{Key, KeyEvent, KeyModifiers},
    props::{Alignment, Borders, Color, Style},
    tui::{
        layout::{Constraint, Rect},
        style::Modifier,
        widgets::{Cell, Row, Table, TableState},
    },
    AttrValue, Attribute, Component, Event, Frame, MockComponent, Props, State,
};

use super::{super::client::state::SlidingSyncState, get_block, JackInEvent, Msg};

/// ## Rooms
pub struct Rooms {
    props: Props,
    sstate: SlidingSyncState,
    tablestate: TableState,
}

impl Rooms {
    pub fn new(sstate: SlidingSyncState) -> Self {
        Self { props: Props::default(), sstate, tablestate: Default::default() }
    }

    pub fn set_sliding_sync(&mut self, sstate: SlidingSyncState) {
        self.sstate = sstate;
    }

    pub fn select_dir(&mut self, count: i32) {
        let rooms_count = self.sstate.view().get_rooms(None, None).len() as i32;
        let current = self.tablestate.selected().unwrap_or_default() as i32;
        let next = {
            let next = current + count;
            if next >= rooms_count {
                next - rooms_count
            } else if next < 0 {
                rooms_count + next
            } else {
                next
            }
        };
        self.tablestate.select(Some(next.try_into().unwrap_or_default()));
    }

    pub fn borders(mut self, b: Borders) -> Self {
        self.attr(Attribute::Borders, AttrValue::Borders(b));
        self
    }
}

impl MockComponent for Rooms {
    fn view(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = ("Rooms".to_owned(), Alignment::Center);

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();
        let focus = self.props.get_or(Attribute::Focus, AttrValue::Flag(false)).unwrap_flag();

        let mut paras = vec![];

        for r in self.sstate.view().get_rooms(None, None) {
            let mut cells = vec![Cell::from(r.name.unwrap_or_else(|| "unknown".to_string()))];
            if let Some(c) = r.unread_notifications.notification_count {
                let count: u32 = c.try_into().unwrap_or_default();
                if count > 0 {
                    cells.push(Cell::from(c.to_string()))
                }
            }
            paras.push(Row::new(cells));
        }

        frame.render_stateful_widget(
            Table::new(paras)
                .style(Style::default().fg(Color::White))
                .highlight_style(
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::ITALIC),
                )
                .highlight_symbol(">>")
                .widths(&[Constraint::Min(30), Constraint::Max(4)])
                .block(get_block(borders, title, focus)),
            area,
            &mut self.tablestate,
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

impl Component<Msg, JackInEvent> for Rooms {
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
                Event::Keyboard(KeyEvent { code: Key::Enter, modifiers: KeyModifiers::NONE }) => {
                    if let Some(idx) = self.tablestate.selected() {
                        if let Some(room_id) = self.sstate.view().get_room_id(idx) {
                            warn!("selecting, {:?}", room_id);
                            return Some(Msg::SelectRoom(Some(room_id)));
                        }
                    }
                    return None;
                }
                Event::Keyboard(KeyEvent { code: Key::Tab, modifiers: KeyModifiers::NONE }) => {
                    return Some(Msg::RoomsBlur)
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
