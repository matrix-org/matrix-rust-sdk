use itertools::Itertools;
use ratatui::{prelude::*, widgets::WidgetRef};

pub mod create_room;
pub mod help;
pub mod popup_input;
pub mod recovery;
pub mod room_list;
pub mod room_view;
pub mod search;
pub mod settings;
pub mod status;

/// A hyperlink widget that renders a hyperlink in the terminal using [OSC 8].
///
/// [OSC 8]: https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
struct Hyperlink<'content> {
    text: Text<'content>,
    url: String,
}

impl<'content> Hyperlink<'content> {
    fn new(text: impl Into<Text<'content>>, url: impl Into<String>) -> Self {
        Self { text: text.into(), url: url.into() }
    }
}

impl WidgetRef for Hyperlink<'_> {
    fn render_ref(&self, area: Rect, buffer: &mut Buffer) {
        self.text.render_ref(area, buffer);

        // this is a hacky workaround for https://github.com/ratatui-org/ratatui/issues/902, a bug
        // in the terminal code that incorrectly calculates the width of ANSI escape
        // sequences. It works by rendering the hyperlink as a series of
        // 2-character chunks, which is the calculated width of the hyperlink
        // text.
        for (i, two_chars) in self.text.to_string().chars().chunks(2).into_iter().enumerate() {
            let text = two_chars.collect::<String>();
            let hyperlink = format!("\x1B]8;;{}\x07{}\x1B]8;;\x07", self.url, text);
            buffer[(area.x + i as u16 * 2, area.y)].set_symbol(hyperlink.as_str());
        }
    }
}
