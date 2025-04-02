use crossterm::event::KeyEvent;
use matrix_sdk_ui::room_list_service::Room;
use ratatui::{prelude::*, widgets::*};
use style::palette::tailwind;
use tui_textarea::TextArea;

/// A widget representing a text input to send messages to a room.
#[derive(Default)]
pub struct Input {
    /// The text area that will keep track of what the user has input.
    textarea: TextArea<'static>,
}

impl Input {
    /// Create a new empty [`Input`] widget.
    pub fn new() -> Self {
        let textarea = TextArea::default();

        Self { textarea }
    }

    /// Receive a key press event and handle it.
    pub fn handle_key_press(&mut self, event: KeyEvent) {
        self.textarea.input(event);
    }

    /// Get the currently input text.
    pub fn get_text(&self) -> String {
        // TODO: Parse some commands using clap here and return an enum that might be a
        // string or a parsed command.
        self.textarea.lines().join("\n")
    }

    /// Is the input area empty, returns false if the user hasn't input anything
    /// yet.
    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    /// Clear the text from the input area.
    pub fn clear(&mut self) {
        self.textarea = TextArea::default();
    }
}

impl<'a> StatefulWidget for &'a mut Input {
    type State = Option<&'a Room>;

    fn render(self, area: Rect, buf: &mut Buffer, room: &mut Self::State)
    where
        Self: Sized,
    {
        // Set the placeholder text depending on the encryption state of the room.
        //
        // We assume that the encryption state is synced because the RoomListService
        // sets it as required state.
        if let Some(room) = room.as_deref() {
            let is_encrypted = room.encryption_state().is_encrypted();

            if is_encrypted {
                self.textarea.set_placeholder_text("(Send an encrypted message)");
            } else {
                self.textarea.set_placeholder_text("(Send an unencrypted message)");
            }
        } else {
            self.textarea.set_placeholder_text("(No room selected)");
        }

        // Let's first create a block to set the background color.
        let input_block = Block::new().borders(Borders::NONE).bg(tailwind::BLUE.c400);

        // Now we set the block and we render the textarea.
        self.textarea.set_block(input_block);
        self.textarea.render(area, buf);
    }
}
