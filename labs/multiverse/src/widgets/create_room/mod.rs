use crossterm::event::KeyEvent;
use ratatui::prelude::*;

use crate::widgets::popup_input::PopupInput;

pub enum RoomEncryption {
    Yes,
    No,
}

#[derive(Default)]
pub struct CreateRoomView {
    input: PopupInput,
}

impl CreateRoomView {
    pub fn new(encrypted: RoomEncryption) -> Self {
        let title = match encrypted {
            RoomEncryption::Yes => "Create Encrypted Room",
            RoomEncryption::No => "Create Room",
        };
        Self { input: PopupInput::new(title, "(Enter name for new room)") }
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
