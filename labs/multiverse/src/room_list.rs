use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use imbl::Vector;
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk_ui::room_list_service;
use ratatui::{prelude::*, widgets::*};

use crate::{ALT_ROW_COLOR, HEADER_BG, NORMAL_ROW_COLOR, SELECTED_STYLE_FG, TEXT_COLOR};

/// Extra room information, like its display name, etc.
#[derive(Clone)]
pub struct ExtraRoomInfo {
    /// Content of the raw m.room.name event, if available.
    pub raw_name: Option<String>,

    /// Calculated display name for the room.
    pub display_name: Option<String>,

    /// Is the room a DM?
    pub is_dm: Option<bool>,
}

pub type Rooms = Arc<Mutex<Vector<room_list_service::Room>>>;
pub type RoomInfos = Arc<Mutex<HashMap<OwnedRoomId, ExtraRoomInfo>>>;

#[derive(Default)]
pub struct RoomList {
    pub state: ListState,
    pub rooms: Rooms,
    /// Extra information about rooms.
    room_infos: RoomInfos,
}

impl RoomList {
    pub fn new(rooms: Rooms, room_infos: RoomInfos) -> Self {
        Self { state: Default::default(), rooms, room_infos }
    }

    /// Focus the list on the next item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    pub fn next(&mut self) -> Option<usize> {
        let num_items = self.rooms.lock().unwrap().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return None;
        }

        // Otherwise, select the next one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i >= num_items - 1 { 0 } else { i + 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            Some(new)
        } else {
            None
        }
    }

    /// Focus the list on the previous item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    pub fn previous(&mut self) -> Option<usize> {
        let num_items = self.rooms.lock().unwrap().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return None;
        }

        // Otherwise, select the previous one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i == 0 { num_items - 1 } else { i - 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            Some(new)
        } else {
            None
        }
    }
}

impl Widget for &mut RoomList {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        // We create two blocks, one is for the header (outer) and the other is for list
        // (inner).
        let outer_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room list")
            .title_alignment(Alignment::Center);
        let inner_block =
            Block::default().borders(Borders::NONE).fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR);

        // We get the inner area from outer_block. We'll use this area later to render
        // the table.
        let outer_area = area;
        let inner_area = outer_block.inner(outer_area);

        // We can render the header in outer_area.
        outer_block.render(outer_area, buf);

        // Don't keep this lock too long by cloning the content. RAM's free these days,
        // right?
        let mut room_info = self.room_infos.lock().unwrap().clone();

        // Iterate through all elements in the `items` and stylize them.
        let items: Vec<ListItem<'_>> = self
            .rooms
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, room)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };

                let line = {
                    let room_id = room.room_id();
                    let room_info = room_info.remove(room_id);

                    let (raw, display, is_dm) = if let Some(info) = room_info {
                        (info.raw_name, info.display_name, info.is_dm)
                    } else {
                        (None, None, None)
                    };

                    let dm_marker = if is_dm.unwrap_or(false) { "🤫" } else { "" };

                    let room_name = if let Some(n) = display {
                        format!("{n} ({room_id})")
                    } else if let Some(n) = raw {
                        format!("m.room.name:{n} ({room_id})")
                    } else {
                        room_id.to_string()
                    };

                    format!("#{i}{dm_marker} {}", room_name)
                };

                let line = Line::styled(line, TEXT_COLOR);
                ListItem::new(line).bg(bg_color)
            })
            .collect();

        // Create a List from all list items and highlight the currently selected one.
        let items = List::new(items)
            .block(inner_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
                    .fg(SELECTED_STYLE_FG),
            )
            .highlight_symbol(">")
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(items, inner_area, buf, &mut self.state);
    }
}
