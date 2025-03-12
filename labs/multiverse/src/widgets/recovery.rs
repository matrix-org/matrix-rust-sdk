use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Padding, Row, Table, TableState},
};

use crate::popup_area;

pub struct RecoveryView {}

impl Widget for &mut RecoveryView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        todo!()
    }
}
