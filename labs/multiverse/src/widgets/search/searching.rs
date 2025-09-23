use crossterm::event::KeyEvent;
use matrix_sdk::{
    deserialized_responses::TimelineEvent,
    ruma::{
        OwnedUserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::message::{MessageType, SyncRoomMessageEvent},
        },
    },
};
use ratatui::{
    layout::Flex,
    prelude::*,
    symbols::border::Set,
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use tui_widget_list::{ListBuilder, ListState, ListView};

use crate::{
    ALT_ROW_COLOR, HEADER_BG, NORMAL_ROW_COLOR, SELECTED_STYLE_FG, TEXT_COLOR,
    widgets::popup_input::{PopupInput, PopupInputBuilder},
};

const MESSAGE_PADDING_LEFT: u16 = 2;
const MESSAGE_PADDING_RIGHT: u16 = 1;
const MESSAGE_PADDING_TOP: u16 = 0;
const MESSAGE_PADDING_BOTTOM: u16 = 0;

#[derive(Default)]
pub struct SearchingView {
    input: PopupInput,
    results: Option<Vec<(OwnedUserId, String, String)>>,
    pub(crate) list_state: ListState,
}

impl SearchingView {
    pub fn new() -> Self {
        let border_set = Set { bottom_left: "╟", bottom_right: "╢", ..symbols::border::PLAIN };

        Self {
            input: PopupInputBuilder::new("", "(Enter search query)")
                .height_constraint(Constraint::Percentage(100))
                .width_constraint(Constraint::Percentage(100))
                .border_set(border_set)
                .borders(Borders::BOTTOM)
                .bg(HEADER_BG)
                .build(),
            results: None,
            list_state: ListState::default(),
        }
    }

    pub fn results(&mut self, values: Vec<TimelineEvent>) {
        let values: Vec<(OwnedUserId, String, String)> =
            values.iter().filter_map(get_message_from_timeline_event).collect();

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
        let [area] = Layout::vertical([Constraint::Percentage(70)]).flex(Flex::Center).areas(area);
        let [area] =
            Layout::horizontal([Constraint::Percentage(70)]).flex(Flex::Center).areas(area);

        let block = Block::bordered()
            .title("Search")
            .title_alignment(Alignment::Center)
            .border_style(Style::new().bg(HEADER_BG))
            .border_type(BorderType::Double);
        let inner_area = block.inner(area);

        let [search_area, results_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(inner_area);

        let messages = if let Some(results) = &self.results
            && !results.is_empty()
        {
            results
                .iter()
                .map(|(sender, time, message)| {
                    MessageWidget::new(sender.to_string(), time.clone(), message.clone())
                })
                .collect()
        } else {
            vec![MessageWidget::new("", "", "No results found!")]
        };

        let count = messages.len();

        let builder = ListBuilder::new(move |context| {
            let mut message_widget = messages[context.index].clone();
            // -2 for the border width.
            let width = results_area.width - 2 - MESSAGE_PADDING_LEFT - MESSAGE_PADDING_RIGHT;
            // +2 for the border with, +1 for safety.
            let main_axis_size =
                textwrap::wrap(&message_widget.content, textwrap::Options::new(width as usize))
                    .len()
                    + 3;

            if context.index % 2 == 0 {
                message_widget.style = Style::default().fg(TEXT_COLOR).bg(ALT_ROW_COLOR);
            } else {
                message_widget.style = Style::default().fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR);
            }

            if context.is_selected {
                message_widget.style = Style::default().bg(NORMAL_ROW_COLOR).fg(SELECTED_STYLE_FG);
            }

            (message_widget, main_axis_size as u16)
        });

        let list = ListView::new(builder, count);

        block.render(area, buf);

        Clear.render(results_area, buf);
        Block::new().borders(Borders::NONE).bg(HEADER_BG).render(results_area, buf);

        list.render(results_area, buf, &mut self.list_state);

        self.input.render(search_area, buf);
    }
}

fn get_message_from_timeline_event(ev: &TimelineEvent) -> Option<(OwnedUserId, String, String)> {
    if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncRoomMessageEvent::Original(ev),
    ))) = ev.raw().deserialize()
        && let MessageType::Text(content) = &ev.content.msgtype
    {
        let time = format!("{:?}", ev.origin_server_ts);

        return Some((ev.sender.to_owned(), time, content.body.clone()));
    }
    None
}

#[derive(Debug, Clone)]
pub struct MessageWidget {
    title: String,
    time: String,
    content: String,
    style: Style,
}

impl MessageWidget {
    pub fn new<T: Into<String>>(title: T, time: T, content: T) -> Self {
        Self {
            title: title.into(),
            time: time.into(),
            content: content.into(),
            style: Style::default().fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR),
        }
    }
}

impl Widget for MessageWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .title(self.title)
            .title_top(Line::from(self.time).right_aligned())
            .padding(Padding::new(
                MESSAGE_PADDING_LEFT,
                MESSAGE_PADDING_RIGHT,
                MESSAGE_PADDING_TOP,
                MESSAGE_PADDING_BOTTOM,
            ));
        Paragraph::new(Text::from(self.content))
            .wrap(Wrap { trim: true })
            .block(block)
            .style(self.style)
            .render(area, buf);
    }
}
