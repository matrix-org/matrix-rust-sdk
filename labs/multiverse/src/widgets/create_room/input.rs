use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Stylize, palette::tailwind},
    widgets::{Block, Borders, Widget},
};
use tui_textarea::TextArea;

#[derive(Default)]
pub(crate) struct Input {
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
    pub fn handle_key_press(&mut self, key: KeyEvent) {
        self.textarea.input(key);
    }

    /// Get the currently input text.
    pub fn get_input(&self) -> String {
        self.textarea.lines().join("\n")
    }
}

impl Widget for &mut Input {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Set the placeholder text.
        self.textarea.set_placeholder_text("(Enter new room name)");

        // Let's first create a block to set the background color.
        let input_block =
            Block::new().borders(Borders::ALL).bg(tailwind::BLUE.c400).title("Create Room");

        // Now we set the block and we render the textarea.
        self.textarea.set_block(input_block);
        self.textarea.render(area, buf);
    }
}
