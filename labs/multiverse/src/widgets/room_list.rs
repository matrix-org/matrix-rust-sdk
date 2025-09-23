use std::{collections::HashMap, sync::Arc};

use imbl::Vector;
use matrix_sdk::{Client, Room, locks::Mutex, ruma::OwnedRoomId};
use matrix_sdk_ui::sync_service::SyncService;
use ratatui::{prelude::*, widgets::*};

use crate::{
    ALT_ROW_COLOR, HEADER_BG, NORMAL_ROW_COLOR, SELECTED_STYLE_FG, TEXT_COLOR,
    widgets::status::StatusHandle,
};

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

pub type Rooms = Arc<Mutex<Vector<matrix_sdk_ui::room_list_service::RoomListItem>>>;
pub type RoomInfos = Arc<Mutex<HashMap<OwnedRoomId, ExtraRoomInfo>>>;

pub struct RoomList {
    pub state: ListState,

    pub status_handle: StatusHandle,

    pub rooms: Rooms,

    client: Client,

    /// Extra information about rooms.
    room_infos: RoomInfos,

    /// The current room that's subscribed to in the room list's sliding sync.
    current_room_subscription: Option<Room>,

    /// The sync service used for synchronizing events.
    sync_service: Arc<SyncService>,
}

impl RoomList {
    pub fn new(
        client: Client,
        rooms: Rooms,

        room_infos: RoomInfos,
        sync_service: Arc<SyncService>,
        status_handle: StatusHandle,
    ) -> Self {
        Self {
            client,
            state: Default::default(),
            rooms,
            status_handle,
            room_infos,
            current_room_subscription: None,
            sync_service,
        }
    }

    /// Focus the list on the next item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    pub async fn next_room(&mut self) {
        let num_items = self.rooms.lock().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return;
        }

        // Otherwise, select the next one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i >= num_items - 1 { 0 } else { i + 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            self.subscribe_to_room(new).await;
        }
    }

    /// Focus the list on the previous item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    pub async fn previous_room(&mut self) {
        let num_items = self.rooms.lock().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return;
        }

        // Otherwise, select the previous one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i == 0 { num_items - 1 } else { i - 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            self.subscribe_to_room(new).await;
        }
    }

    /// Returns the [`OwnedRoomId`] of the `nth` room within the [`RoomList`].
    pub fn get_room_id_of_entry(&self, nth: usize) -> Option<OwnedRoomId> {
        self.rooms.lock().get(nth).cloned().map(|room| room.room_id().to_owned())
    }

    /// Returns the [`OwnedRoomId`] of the currently selected room, if any.
    pub fn get_selected_room_id(&self) -> Option<OwnedRoomId> {
        let selected = self.state.selected()?;
        self.get_room_id_of_entry(selected)
    }

    /// Subscribe to room that is shown at the given `index`.
    async fn subscribe_to_room(&mut self, index: usize) {
        // Cancel the subscription to the previous room, if any.
        self.current_room_subscription.take();

        // Subscribe to the new room.
        if let Some(room) =
            self.get_room_id_of_entry(index).and_then(|room_id| self.client.get_room(&room_id))
        {
            self.sync_service.room_list_service().subscribe_to_rooms(&[room.room_id()]).await;
            self.current_room_subscription = Some(room);
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
            .borders(Borders::RIGHT)
            .border_set(symbols::border::THICK)
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
        let mut room_info = self.room_infos.lock().clone();

        // Iterate through all elements in the `items` and stylize them.
        let items: Vec<ListItem<'_>> = self
            .rooms
            .lock()
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

                    format!("#{i}{dm_marker} {room_name}")
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
