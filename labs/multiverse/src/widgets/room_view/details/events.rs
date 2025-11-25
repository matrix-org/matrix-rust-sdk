use itertools::Itertools;
use matrix_sdk::Room;
use matrix_sdk_common::executor::Handle;
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Wrap},
};

use crate::TEXT_COLOR;

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
        match self.room {
            Some(room) => {
                let events = tokio::task::block_in_place(|| {
                    Handle::current().block_on(async {
                        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
                        room_event_cache
                            .events()
                            .await
                            .expect("Failed to fetch the events because of a store error")
                    })
                });

                let separator = Line::from("\n");
                let events = events
                    .into_iter()
                    .map(|sync_timeline_item| sync_timeline_item.raw().json().to_string())
                    .map(Line::from);

                let events = Itertools::intersperse(events, separator);
                let lines: Vec<_> = [Line::from("")].into_iter().chain(events).collect();

                Paragraph::new(lines).fg(TEXT_COLOR).wrap(Wrap { trim: false }).render(area, buf);
            }

            None => {
                Paragraph::new("(room disappeared in the room list service)")
                    .fg(TEXT_COLOR)
                    .wrap(Wrap { trim: false })
                    .render(area, buf);
            }
        }
    }
}
