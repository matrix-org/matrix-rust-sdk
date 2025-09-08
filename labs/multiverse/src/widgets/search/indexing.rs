use ratatui::{
    layout::Flex,
    prelude::*,
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};
use throbber_widgets_tui::ThrobberState;

use crate::{HEADER_BG, NORMAL_ROW_COLOR, TEXT_COLOR, popup_area};

pub enum IndexingMessage {
    Progress(usize),
    Error(String),
}

impl Default for IndexingMessage {
    fn default() -> Self {
        IndexingMessage::Progress(0)
    }
}

#[derive(Default)]
pub struct IndexingView {
    throbber_state: ThrobberState,
    message: IndexingMessage,
}

impl IndexingView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_tick(&mut self) {
        self.throbber_state.calc_next();
    }

    pub fn set_message(&mut self, message: IndexingMessage) {
        self.message = message;
    }
}

impl Widget for &mut IndexingView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let outer_area = popup_area(area, 80, 80);
        let block = Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(TEXT_COLOR).bg(HEADER_BG))
            .style(Style::default().fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR));
        Clear.render(outer_area, buf);

        let inner_area = block.inner(outer_area);
        block.render(outer_area, buf);

        match &self.message {
            IndexingMessage::Error(err) => {
                let message = format!(
                    "An error occurred during indexing, press Escape to return to main screen: {err}"
                );

                let [area] = Layout::horizontal([Constraint::Length(message.len() as u16)])
                    .flex(Flex::Center)
                    .areas(inner_area);
                let [area] =
                    Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(area);

                let paragraph = Paragraph::new(message).wrap(Wrap { trim: true });

                paragraph.render(area, buf);
            }
            IndexingMessage::Progress(progress) => {
                let label = format!("Indexing...({})", progress);

                let [area] = Layout::horizontal([Constraint::Length(label.len() as u16)])
                    .flex(Flex::Center)
                    .areas(inner_area);
                let [area] =
                    Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(area);

                let throbber = throbber_widgets_tui::Throbber::default()
                    .label(label)
                    .style(Style::default().fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR))
                    .throbber_style(Style::default().fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR))
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
                    .use_type(throbber_widgets_tui::WhichUse::Spin);

                StatefulWidget::render(throbber, area, buf, &mut self.throbber_state);
            }
        }
    }
}
