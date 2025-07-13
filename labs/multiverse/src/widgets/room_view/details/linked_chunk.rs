use matrix_sdk::Room;
use matrix_sdk_common::executor::Handle;
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Wrap},
};

use crate::TEXT_COLOR;

pub struct LinkedChunkView<'a> {
    room: Option<&'a Room>,
}

impl<'a> LinkedChunkView<'a> {
    pub fn new(room: Option<&'a Room>) -> Self {
        Self { room }
    }
}

impl Widget for &mut LinkedChunkView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        match self.room {
            Some(room) => {
                let lines = tokio::task::block_in_place(|| {
                    Handle::current().block_on(async {
                        let (cache, _drop_guards) =
                            room.event_cache().await.expect("no event cache for that room");
                        cache.debug_string().await
                    })
                });

                let lines: Vec<Line<'_>> = lines.into_iter().map(Line::from).collect();

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
