use tui_realm_stdlib::{states::InputStates, Input};
use tuirealm::{
    command::{Cmd, Direction, Position},
    event::{Key, KeyEvent, KeyModifiers},
    props::{Alignment, BorderType, Borders, Color, InputType, Style},
    Component, Event, MockComponent,
};

use super::{JackInEvent, Msg};

#[derive(MockComponent)]
pub struct InputText {
    component: Input,
}

impl Default for InputText {
    fn default() -> Self {
        Self {
            component: Input::default()
                .borders(
                    Borders::default().modifiers(BorderType::Rounded).color(Color::LightYellow),
                )
                .foreground(Color::LightYellow)
                .input_type(InputType::Text)
                .title("Send a message", Alignment::Left)
                .invalid_style(Style::default().fg(Color::Red)),
        }
    }
}

impl Component<Msg, JackInEvent> for InputText {
    fn on(&mut self, ev: Event<JackInEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent { code: Key::Left, .. }) => {
                self.perform(Cmd::Move(Direction::Left));
            }
            Event::Keyboard(KeyEvent { code: Key::Right, .. }) => {
                self.perform(Cmd::Move(Direction::Right));
            }
            Event::Keyboard(KeyEvent { code: Key::Home, .. }) => {
                self.perform(Cmd::GoTo(Position::Begin));
            }
            Event::Keyboard(KeyEvent { code: Key::End, .. }) => {
                self.perform(Cmd::GoTo(Position::End));
            }
            Event::Keyboard(KeyEvent { code: Key::Delete, .. }) => {
                self.perform(Cmd::Cancel);
            }
            Event::Keyboard(KeyEvent { code: Key::Backspace, .. }) => {
                self.perform(Cmd::Delete);
            }
            Event::Keyboard(KeyEvent { code: Key::Char(ch), modifiers: KeyModifiers::NONE }) => {
                self.perform(Cmd::Type(ch));
            }
            Event::Keyboard(KeyEvent { code: Key::Enter, .. }) => {
                let input = self.component.states.get_value();
                self.component.states = InputStates::default();
                return Some(Msg::SendMessage(input));
            }
            Event::Keyboard(KeyEvent { code: Key::Tab, .. }) => {
                return Some(Msg::TextBlur);
            }
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => {
                return Some(Msg::AppClose);
            }
            _ => {}
        }
        None
    }
}
