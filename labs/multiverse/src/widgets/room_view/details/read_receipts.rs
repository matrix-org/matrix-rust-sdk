use matrix_sdk::Room;
use matrix_sdk_base::read_receipts::RoomReadReceipts;
use matrix_sdk_ui::timeline::TimelineItem;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{TEXT_COLOR, widgets::room_view::DetailsState};

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

impl Widget for &mut ReadReceipts<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        if let Some(room) = self.state.selected_room {
            match self.state.selected_item.as_deref() {
                Some(selected_event) => {
                    if let Some(item) = format_timeline_item(selected_event) {
                        Paragraph::new(item)
                            .fg(TEXT_COLOR)
                            .wrap(Wrap { trim: false })
                            .block(Block::new().borders(Borders::BOTTOM))
                            .render(area, buf);
                    } else {
                        render_room_stats(room, area, buf);
                    }
                }
                None => render_room_stats(room, area, buf),
            }
        } else {
            let content = "(room disappeared in the room list service)";
            Paragraph::new(content).fg(TEXT_COLOR).wrap(Wrap { trim: false }).render(area, buf);
        }
    }
}

fn format_timeline_item(item: &TimelineItem) -> Option<Vec<Line<'_>>> {
    let event = item.as_event()?;
    let receipts = event.read_receipts();
    let sender = event.sender();
    let event_id = event.event_id();

    let first_line = Line::from(format!("{sender} - {event_id:?}"));
    let second_line = Line::from(format!("{receipts:?}"));

    Some(vec![first_line, second_line])
}
