use crossterm::event::KeyEvent;
use matrix_sdk::ruma::OwnedEventId;
use ratatui::{
    layout::Flex,
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Padding, Row, Table, TableState},
};

use crate::widgets::popup_input::PopupInput;

#[derive(Default)]
pub struct SearchingView {
    input: PopupInput,
    results: Option<Vec<OwnedEventId>>,
}

impl SearchingView {
    pub fn new() -> Self {
        Self {
            input: PopupInput::new("Search", "(Enter search query)")
                .height_constraint(Constraint::Percentage(100))
                .width_constraint(Constraint::Percentage(100)),
            results: None,
        }
    }

    pub fn results(&mut self, values: Vec<OwnedEventId>) {
        self.results = Some(values);
    }

    pub fn get_text(&self) -> Option<String> {
        let name = self.input.get_input();
        if !name.is_empty() { Some(name) } else { None }
    }

    pub fn handle_key_press(&mut self, key: KeyEvent) {
        self.input.handle_key_press(key);
    }
}

impl Widget for &mut SearchingView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let vertical = Layout::vertical([Constraint::Length(3), Constraint::Percentage(80)])
            .flex(Flex::Center);
        let horizontal = Layout::horizontal([Constraint::Percentage(50)]).flex(Flex::Center);
        let [area] = horizontal.areas(area);
        let [search_area, results_area] = vertical.areas(area);

        let rows = if let Some(results) = &self.results {
            results
                .iter()
                .map(|ev_id| Row::new([Cell::new(ev_id.to_string())]))
                .collect::<Vec<Row<'_>>>()
        } else {
            vec![Row::new([Cell::new("No results found!")])]
        };

        let block =
            Block::bordered().title(" Search ").borders(Borders::ALL).padding(Padding::left(2));

        let results_table = Table::new(rows, [Constraint::Percentage(100)]).block(block);

        Clear.render(results_area, buf);
        StatefulWidget::render(results_table, results_area, buf, &mut TableState::default());

        self.input.render(search_area, buf);
    }
}
