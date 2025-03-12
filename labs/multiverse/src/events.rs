use itertools::Itertools;
use matrix_sdk_ui::room_list_service::Room;
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use tokio::runtime::Handle;

use crate::{popup_area, TEXT_COLOR};

pub struct EventsView<'a> {
    room: Option<&'a Room>,
}

impl<'a> EventsView<'a> {
    pub fn new(room: Option<&'a Room>) -> Self {
        Self { room }
    }
}

impl Widget for &mut EventsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let block = Block::bordered().title("Events");
        let area = popup_area(area, 80, 80);
        Clear.render(area, buf);

        match self.room {
            Some(room) => {
                let events = tokio::task::block_in_place(|| {
                    Handle::current().block_on(async {
                        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
                        let (events, _) = room_event_cache.subscribe().await;
                        events
                    })
                });

                let separator = Line::from("\n");
                let events = events
                    .into_iter()
                    .map(|sync_timeline_item| sync_timeline_item.raw().json().to_string())
                    .map(|item| Line::from(item));

                let events = Itertools::intersperse(events, separator);
                let lines: Vec<_> = [Line::from("")].into_iter().chain(events).collect();

                Paragraph::new(lines)
                    .block(block.clone())
                    .fg(TEXT_COLOR)
                    .wrap(Wrap { trim: false })
                    .render(area, buf);
            }

            None => {
                Paragraph::new("(room disappeared in the room list service)")
                    .block(block.clone())
                    .fg(TEXT_COLOR)
                    .wrap(Wrap { trim: false })
                    .render(area, buf);
            }
        }
    }
}
