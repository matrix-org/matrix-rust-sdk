use matrix_sdk_ui::room_list_service::Room;
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::{popup_area, TEXT_COLOR};

pub struct ReadReceipts<'a> {
    room: Option<&'a Room>,
}

impl<'a> ReadReceipts<'a> {
    pub fn new(room: Option<&'a Room>) -> Self {
        Self { room }
    }
}

impl Widget for &mut ReadReceipts<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let read_receipt_block = Block::bordered().title("Read receipts");
        let read_receipt_area = popup_area(area, 80, 80);
        Clear.render(read_receipt_area, buf);

        match self.room {
            Some(room) => {
                let receipts = room.read_receipts();
                let content = format!(
                    r#"Read receipts:
- unread: {}
- notifications: {}
- mentions: {}

---

{:?}
"#,
                    receipts.num_unread,
                    receipts.num_notifications,
                    receipts.num_mentions,
                    receipts
                );

                Paragraph::new(content)
                    .block(read_receipt_block.clone())
                    .fg(TEXT_COLOR)
                    .wrap(Wrap { trim: false })
                    .render(read_receipt_area, buf);
            }
            None => {
                let content = "(room disappeared in the room list service)";
                Paragraph::new(content)
                    .block(read_receipt_block.clone())
                    .fg(TEXT_COLOR)
                    .wrap(Wrap { trim: false })
                    .render(read_receipt_area, buf);
            }
        }

        read_receipt_block.render(read_receipt_area, buf);
    }
}
