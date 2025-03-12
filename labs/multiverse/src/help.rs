use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Padding, Row, Table, TableState},
};

use crate::popup_area;

pub struct HelpView {}

impl HelpView {
    pub fn new() -> Self {
        Self {}
    }
}

impl Widget for &mut HelpView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block =
            Block::bordered().title(" Help Menu ").borders(Borders::ALL).padding(Padding::left(2));
        let area = popup_area(area, 80, 80);
        Clear.render(area, buf);

        let rows = vec![
            Row::new(vec![Cell::from("F1"), Cell::from("Open Help")]),
            Row::new(vec![Cell::from("s"), Cell::from("Resume syncing")]),
            Row::new(vec![Cell::from("S"), Cell::from("Stop syncing")]),
            Row::new(vec![Cell::from("Q"), Cell::from("Enable/disable the send queue")]),
            Row::new(vec![Cell::from("M"), Cell::from("Send a message to the selected room")]),
            Row::new(vec![
                Cell::from("L"),
                Cell::from("Like the last message in the selected room"),
            ]),
        ];
        let widths = [Constraint::Length(5), Constraint::Length(5)];

        let help_table = Table::new(rows, widths)
            .block(block)
            .widths(&[Constraint::Length(10), Constraint::Min(30)]);

        StatefulWidget::render(help_table, area, buf, &mut TableState::default());
    }
}
