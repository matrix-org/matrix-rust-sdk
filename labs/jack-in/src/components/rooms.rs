
use super::{Msg, get_block};

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::tui::{
        layout::{Rect, Constraint},
        widgets::{Table, Row, Cell},
        style::Modifier
};
use tuirealm::{
    AttrValue, Attribute, Component, Event, Frame, MockComponent, NoUserEvent, Props, State,
};
use tuirealm::props::{Alignment, Borders, Color, Style, TextModifiers};
use super::super::client::state::SlidingSyncState;

/// ## Rooms
pub struct Rooms {
    props: Props,
    sstate: SlidingSyncState
}

impl Rooms {
    pub fn new(sstate: SlidingSyncState) -> Self {
        Self {
            props: Props::default(),
            sstate,
        }
    }
}

impl MockComponent for Rooms {
    fn view(&mut self, frame: &mut Frame, area: Rect) {

        let title = ("Rooms".to_owned(), Alignment::Center);

        let borders = self
            .props
            .get_or(Attribute::Borders, AttrValue::Borders(Borders::default()))
            .unwrap_borders();
        let focus = self
            .props
            .get_or(Attribute::Focus, AttrValue::Flag(false))
            .unwrap_flag();

        
        let mut paras = vec![];

        for r in self.sstate.view().get_rooms(None, None) {
            let mut cells = vec![
                Cell::from(r.name.unwrap_or("unknown".to_string()))
            ];
            if let Some(c) = r.notification_count {
                let count: u32 = c.try_into().unwrap_or_default();
                if count > 0 {
                    cells.push(Cell::from(c.to_string()))
                }
            } 
            paras.push(Row::new(cells));
        }

        frame.render_widget(
            Table::new(paras)
                .style(Style::default().fg(Color::White))
                .highlight_style(Style::default().fg(Color::LightCyan).add_modifier(Modifier::ITALIC))
                .highlight_symbol(">>")
                .widths(&[Constraint::Min(30), Constraint::Max(4)])
                .block(get_block(borders, title, focus)),
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

impl Component<Msg, NoUserEvent> for Rooms {
    fn on(&mut self, _: Event<NoUserEvent>) -> Option<Msg> {
        // Does nothing
        None
    }
}
