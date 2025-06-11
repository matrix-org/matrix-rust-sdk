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

impl Widget for &mut ReadReceipts<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        match self.state.selected_room {
            Some(room) => {
                let RoomReadReceipts { num_unread, num_notifications, num_mentions, .. } =
                    room.read_receipts();

                let vertical = Layout::vertical([Constraint::Length(8), Constraint::Fill(1)]);
                let [top, bottom] = vertical.areas(area);

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
                    .render(top, buf);

                if let Some(items) = self.state.timeline_items {
                    let mut list_items = Vec::new();
                    let mut selected_item = None;

                    for (index, item) in items.into_iter().enumerate() {
                        let Some(event) = item.as_event() else { continue };

                        if event.event_id() == self.state.selected_event.as_deref() {
                            selected_item = Some(index);
                        }

                        let Some(list_item) = format_timeline_item(item) else {
                            continue;
                        };

                        list_items.push(list_item);
                    }

                    let list = List::new(list_items)
                        .highlight_spacing(HighlightSpacing::Always)
                        .highlight_symbol(">")
                        .highlight_style(SELECTED_STYLE_FG);

                    let mut state = ListState::default().with_selected(selected_item);
                    StatefulWidget::render(list, bottom, buf, &mut state);
                }
            }
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
