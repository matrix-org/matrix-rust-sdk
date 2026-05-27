#![allow(unused)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::Widget};

#[derive(Debug, Clone)]
pub struct Button<'text> {
    text: Text<'text>,
    theme: Theme,
    state: State,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum State {
    #[default]
    Normal,
    Selected,
    Pressed,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    normal_text: Color,
    normal_background: Color,
    selected_text: Color,
    selected_background: Color,
    pressed_text: Color,
    pressed_background: Color,
    highlight: Color,
    shadow: Color,
}

impl Default for Theme {
    fn default() -> Self {
        themes::NORMAL
    }
}

/// Config
impl<'text> Button<'text> {
    pub fn new<T: Into<Text<'text>>>(text: T) -> Self {
        Self { text: text.into(), theme: Theme::default(), state: State::default() }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }
}

impl Button<'_> {
    pub fn toggle_press(&mut self) {
        match self.state {
            State::Normal => self.press(),
            State::Selected => self.press(),
            State::Pressed => self.select(),
        }
    }

    pub fn press(&mut self) {
        self.state = State::Pressed;
    }

    pub fn normal(&mut self) {
        self.state = State::Normal;
    }

    pub fn select(&mut self) {
        self.state = State::Selected;
    }

    fn handle_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_press(),
            _ => {}
        }
    }
}

impl Widget for &Button<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let theme = self.theme;

        // these are wrong
        let fg = match self.state {
            State::Normal => theme.normal_text,
            State::Selected => theme.selected_text,
            State::Pressed => theme.pressed_text,
        };
        let bg = match self.state {
            State::Normal => theme.normal_background,
            State::Selected => theme.selected_background,
            State::Pressed => theme.pressed_background,
        };
        let (top, bottom) = if self.state == State::Pressed {
            (theme.shadow, theme.highlight)
        } else {
            (theme.highlight, theme.shadow)
        };

        buf.set_style(area, (fg, bg));

        let rows = area.rows().collect::<Vec<_>>();
        let last_index = rows.len().saturating_sub(1);
        let (first, middle, last) = match rows.len() {
            0 | 1 => (None, &rows[..], None),
            2 => (None, &rows[..last_index], Some(rows[last_index])),
            _ => (Some(rows[0]), &rows[1..last_index], Some(rows[last_index])),
        };

        // render top line if there's enough space
        if let Some(first) = first {
            "▔".repeat(area.width as usize).fg(top).bg(bg).render(first, buf);
        }
        // render bottom line if there's enough space
        if let Some(last) = last {
            "▁".repeat(area.width as usize).fg(bottom).bg(bg).render(last, buf);
        }
        self.text.clone().centered().render(middle[0], buf);
    }
}

pub mod themes {
    use ratatui::style::palette::tailwind;

    use super::Theme;

    pub const NORMAL: Theme = Theme {
        normal_text: tailwind::GRAY.c200,
        normal_background: tailwind::GRAY.c800,
        selected_text: tailwind::GRAY.c100,
        selected_background: tailwind::GRAY.c700,
        pressed_text: tailwind::GRAY.c300,
        pressed_background: tailwind::GRAY.c900,
        highlight: tailwind::GRAY.c600,
        shadow: tailwind::GRAY.c950,
    };

    pub const RED: Theme = Theme {
        normal_text: tailwind::RED.c200,
        normal_background: tailwind::RED.c800,
        selected_text: tailwind::RED.c100,
        selected_background: tailwind::RED.c700,
        pressed_text: tailwind::RED.c300,
        pressed_background: tailwind::RED.c900,
        highlight: tailwind::RED.c600,
        shadow: tailwind::RED.c950,
    };

    pub const GREEN: Theme = Theme {
        normal_text: tailwind::GREEN.c200,
        normal_background: tailwind::GREEN.c800,
        selected_text: tailwind::GREEN.c100,
        selected_background: tailwind::GREEN.c700,
        pressed_text: tailwind::GREEN.c300,
        pressed_background: tailwind::GREEN.c900,
        highlight: tailwind::GREEN.c600,
        shadow: tailwind::GREEN.c950,
    };

    pub const BLUE: Theme = Theme {
        normal_text: tailwind::BLUE.c200,
        normal_background: tailwind::BLUE.c800,
        selected_text: tailwind::BLUE.c100,
        selected_background: tailwind::BLUE.c700,
        pressed_text: tailwind::BLUE.c300,
        pressed_background: tailwind::BLUE.c900,
        highlight: tailwind::BLUE.c600,
        shadow: tailwind::BLUE.c950,
    };
}
