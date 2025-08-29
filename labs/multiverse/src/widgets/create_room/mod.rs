use crossterm::event::KeyEvent;
use ratatui::prelude::*;

use crate::widgets::popup_input::{PopupInput, PopupInputBuilder};

#[derive(Default)]
pub struct CreateRoomView {
    input: PopupInput,
}

impl CreateRoomView {
    pub fn new() -> Self {
        Self { input: PopupInputBuilder::new("Create Room", "(Enter name for new room)").build() }
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
        self.input.render(area, buf);
    }
}
