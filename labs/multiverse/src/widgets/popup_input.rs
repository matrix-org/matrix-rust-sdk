use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Stylize, palette::tailwind},
    widgets::{Block, Borders, Clear, Widget},
};
use tui_textarea::TextArea;

#[derive(Default, Clone)]
pub(crate) struct PopupInputBuilder {
    /// Title of popup
    title: String,

    /// Placeholder text of popup
    placeholder_text: String,

    /// Width constraint text of popup
    width_constraint: Constraint,

    /// Height constraint of popup
    height_constraint: Constraint,

    /// Optional border characters used for collapsed borders
    border_set: Option<ratatui::symbols::border::Set>,

    /// Borders
    borders: Option<Borders>,

    /// Background color
    bg: Option<Color>,

    /// Foreground color
    fg: Option<Color>,
}

impl PopupInputBuilder {
    /// Create a new empty [`PopupInput`] widget.
    pub fn new<T: Into<String>>(title: T, placeholder_text: T) -> Self {
        Self {
            title: title.into(),
            placeholder_text: placeholder_text.into(),
            width_constraint: Constraint::Percentage(40),
            height_constraint: Constraint::Length(3),
            ..Default::default()
        }
    }

    /// Width constraint text of popup
    pub fn width_constraint(&mut self, value: Constraint) -> &mut Self {
        self.width_constraint = value;
        self
    }

    /// Height constraint of popup
    pub fn height_constraint(&mut self, value: Constraint) -> &mut Self {
        self.height_constraint = value;
        self
    }

    /// Set the custom border characters
    pub fn border_set(&mut self, set: ratatui::symbols::border::Set) -> &mut Self {
        self.border_set = Some(set);
        self
    }

    /// Set the borders
    pub fn borders(&mut self, borders: Borders) -> &mut Self {
        self.borders = Some(borders);
        self
    }

    /// Set the background color
    pub fn bg(&mut self, color: Color) -> &mut Self {
        self.bg = color.into();
        self
    }

    /// Build the [`PopupInput`] from this builder
    pub fn build(&self) -> PopupInput {
        let mut ret = PopupInput {
            textarea: TextArea::default(),
            height_constraint: self.height_constraint,
            width_constraint: self.width_constraint,
        };

        ret.textarea.set_placeholder_text(self.placeholder_text.clone());

        let border_set = self.border_set.unwrap_or_default();
        let borders = self.borders.unwrap_or_default();
        let bg = self.bg.unwrap_or(tailwind::BLUE.c400);
        let fg = self.fg.unwrap_or(tailwind::GRAY.c50);

        let input_block = Block::new()
            .border_set(border_set)
            .borders(borders)
            .bg(bg)
            .fg(fg)
            .title(self.title.clone());

        ret.textarea.set_block(input_block);

        ret
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PopupInput {
    /// Input field for text
    textarea: TextArea<'static>,

    /// Height constraint of input
    height_constraint: Constraint,

    /// Width constraint of input
    width_constraint: Constraint,
}

impl PopupInput {
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
