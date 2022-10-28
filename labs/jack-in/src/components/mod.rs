//! ## Components
//!
//! demo example components

use tuirealm::{
    props::{Alignment, Borders, Color, Style},
    tui::widgets::Block,
};

use super::{JackInEvent, Msg};

// -- modules
mod details;
mod input;
mod label;
mod logger;
mod rooms;
mod statusbar;

// -- export
pub use details::Details;
pub use input::InputText;
pub use label::Label;
pub use logger::Logger;
pub use rooms::Rooms;
pub use statusbar::StatusBar;

/// ### get_block
///
/// Get block
pub(crate) fn get_block<'a>(props: Borders, title: (String, Alignment), focus: bool) -> Block<'a> {
    Block::default()
        .borders(props.sides)
        .border_style(match focus {
            true => props.style(),
            false => Style::default().fg(Color::Reset).bg(Color::Reset),
        })
        .border_type(props.modifiers)
        .title(title.0)
        .title_alignment(title.1)
}
