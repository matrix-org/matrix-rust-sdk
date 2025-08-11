use crossterm::event::KeyEvent;
use ratatui::{layout::Flex, prelude::*};
mod input;
use input::Input;

#[derive(Default)]
pub struct CreateRoomView {
    input: Input,
}

impl CreateRoomView {
    pub fn new() -> Self {
        Self { input: Input::new() }
    }

    pub fn get_text(&self) -> Option<String> {
        let name = self.input.get_input();
        if !name.is_empty() { Some(name) } else { None }
    }

    pub fn handle_key_press(&mut self, key: KeyEvent) {
        self.input.handle_key_press(key);
    }
}

impl Widget for &mut CreateRoomView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let vertical = Layout::vertical([Constraint::Length(3)]).flex(Flex::Center);
        let horizontal = Layout::horizontal([Constraint::Percentage(30)]).flex(Flex::Center);
        let [area] = vertical.areas(area);
        let [area] = horizontal.areas(area);

        self.input.render(area, buf);
    }
}
