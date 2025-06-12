use matrix_sdk::{ruma::EventId, Room};
use matrix_sdk_base::read_receipts::RoomReadReceipts;
use matrix_sdk_ui::timeline::TimelineItem;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{widgets::room_view::DetailsState, SELECTED_STYLE_FG, TEXT_COLOR};

pub struct ReadReceipts<'a> {
    state: &'a DetailsState<'a>,
}

impl<'a> ReadReceipts<'a> {
    pub(super) fn new(state: &'a DetailsState<'a>) -> Self {
        Self { state }
    }
}

fn render_room_stats(room: &Room, area: Rect, buf: &mut Buffer) {
    let RoomReadReceipts { num_unread, num_notifications, num_mentions, .. } = room.read_receipts();

    let content = vec![
        Line::from(format!("- unread: {num_unread}")),
        Line::from(format!("- notifications: {num_notifications}")),
        Line::from(format!("- mentions: {num_mentions}")),
        Line::from(""),
        Line::from("---"),
        Line::from(format!("{:?}", room.read_receipts())),
        Line::from("#"),
    ];

    Paragraph::new(content)
        .fg(TEXT_COLOR)
        .wrap(Wrap { trim: false })
        .block(Block::new().borders(Borders::BOTTOM))
        .render(area, buf);
}

fn find_selected_event(item: &TimelineItem, selected_event: &EventId) -> bool {
    match item.as_event() {
        Some(event) => event.event_id() == Some(selected_event),
        None => false,
    }
}

impl Widget for &mut ReadReceipts<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        match self.state.selected_room {
            Some(room) => match (self.state.timeline_items, self.state.selected_event.as_deref()) {
                (None, None) | (None, Some(_)) | (Some(_), None) => {
                    render_room_stats(room, area, buf)
                }
                (Some(items), Some(selected_event)) => {
                    let item =
                        items.into_iter().find(|&item| find_selected_event(item, selected_event));

                    if let Some(item) = item.and_then(|i| format_timeline_item(i)) {
                        let second_item = ListItem::from(selected_event.to_string());

                        let list = List::new(vec![second_item, item])
                            .highlight_spacing(HighlightSpacing::Always)
                            .highlight_symbol(">")
                            .highlight_style(SELECTED_STYLE_FG);

                        let mut state = ListState::default();

                        StatefulWidget::render(list, area, buf, &mut state);
                    } else {
                        render_room_stats(room, area, buf);
                    }
                }
            },
            None => {
                let content = "(room disappeared in the room list service)";
                Paragraph::new(content).fg(TEXT_COLOR).wrap(Wrap { trim: false }).render(area, buf);
            }
        }
    }
}

fn format_timeline_item(item: &TimelineItem) -> Option<ListItem<'_>> {
    let event = item.as_event()?;
    let receipts = event.read_receipts();
    let sender = event.sender();
    let event_id = event.event_id();

    let first_line = Line::from(format!("{sender} - {event_id:?}"));
    let second_line = Line::from(format!("{receipts:?}"));

    Some(ListItem::from(vec![first_line, second_line]))
}
