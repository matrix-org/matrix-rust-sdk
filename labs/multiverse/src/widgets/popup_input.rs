use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Stylize, palette::tailwind},
    widgets::{Block, Borders, Clear, Widget},
};
use tui_textarea::TextArea;

#[derive(Default, Clone)]
pub(crate) struct PopupInput {
    /// The text area that will keep track of what the user has input.
    textarea: TextArea<'static>,

    /// Title of popup
    title: String,

    /// Placeholder text of popup
    placeholder_text: String,

    /// Width constraint text of popup
    width_constraint: Constraint,

    /// Height constraint of popup
    height_constraint: Constraint,
}

impl PopupInput {
    /// Create a new empty [`PopupInput`] widget.
    pub fn new<T: Into<String>>(title: T, placeholder_text: T) -> Self {
        let mut ret = Self {
            textarea: TextArea::default(),
            title: title.into(),
            placeholder_text: placeholder_text.into(),
            width_constraint: Constraint::Percentage(40),
            height_constraint: Constraint::Length(3),
        };

        // Set the placeholder text.
        ret.textarea.set_placeholder_text(ret.placeholder_text.clone());

        // Let's first create a block to set the background color.
        let input_block =
            Block::new().borders(Borders::ALL).bg(tailwind::BLUE.c400).title(ret.title.clone());

        // Now we set the block and we render the textarea.
        ret.textarea.set_block(input_block);

        ret
    }

    /// Width constraint text of popup
    pub fn width_constraint(&mut self, value: Constraint) -> Self {
        self.width_constraint = value;
        self.clone()
    }

    /// Height constraint of popup
    pub fn height_constraint(&mut self, value: Constraint) -> Self {
        self.height_constraint = value;
        self.clone()
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

impl Widget for &mut PopupInput {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let vertical = Layout::vertical([self.height_constraint]).flex(Flex::Center);
        let horizontal = Layout::horizontal([self.width_constraint]).flex(Flex::Center);
        let [area] = vertical.areas(area);
        let [area] = horizontal.areas(area);
        Clear.render(area, buf);

        self.textarea.render(area, buf);
    }
}
