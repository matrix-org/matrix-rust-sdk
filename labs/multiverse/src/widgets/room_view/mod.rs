use std::{ops::Deref, sync::Arc};

use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use matrix_sdk::{
    locks::Mutex,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::room::message::RoomMessageEventContent, OwnedRoomId,
    },
};
use ratatui::{prelude::*, widgets::*};
use tokio::{spawn, task::JoinHandle};

use self::{details::RoomDetails, input::Input, timeline::TimelineView};
use super::status::StatusHandle;
use crate::{
    widgets::recovery::ShouldExit, Timelines, UiRooms, HEADER_BG, NORMAL_ROW_COLOR, TEXT_COLOR,
};

mod details;
mod input;
mod timeline;

#[derive(Default)]
enum Mode {
    #[default]
    Normal,
    Details {
        view: RoomDetails,
    },
}

pub struct RoomView {
    selected_room: Option<OwnedRoomId>,

    /// Room list service rooms known to the app.
    ui_rooms: UiRooms,

    /// Timelines data structures for each room.
    timelines: Timelines,

    status_handle: StatusHandle,

    current_pagination: Arc<Mutex<Option<JoinHandle<()>>>>,

    mode: Mode,

    input: Input,
}

impl RoomView {
    pub fn new(ui_rooms: UiRooms, timelines: Timelines, status_handle: StatusHandle) -> Self {
        Self {
            selected_room: None,
            ui_rooms,
            timelines,
            status_handle,
            current_pagination: Default::default(),
            mode: Mode::default(),
            input: Input::new(),
        }
    }

    pub async fn handle_key_press(&mut self, key: KeyEvent) {
        use KeyCode::*;

        if key.kind != KeyEventKind::Press {
            return;
        }

        match &mut self.mode {
            Mode::Normal => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, Enter) => {
                    if !self.input.is_empty() {
                        let message = self.input.get_text();

                        match self.send_message(message).await {
                            Ok(_) => {
                                self.input.clear();
                            }
                            Err(err) => {
                                self.status_handle
                                    .set_message(format!("error when sending event: {err}"));
                            }
                        }
                    }
                }

                (KeyModifiers::CONTROL, Char('l')) => self.toggle_reaction_to_latest_msg().await,

                (KeyModifiers::NONE, PageUp) => self.back_paginate(),

                (KeyModifiers::CONTROL, Char('d')) => {
                    if self.selected_room.is_some() {
                        self.mode = Mode::Details { view: RoomDetails::new() }
                    }
                }

                _ => self.input.handle_key_press(key),
            },

            Mode::Details { view } => match view.handle_key_press(key) {
                ShouldExit::No => {}
                ShouldExit::OnlySubScreen => {}
                ShouldExit::Yes => self.mode = Mode::Normal,
            },
        }
    }

    pub fn set_selected_room(&mut self, room: Option<OwnedRoomId>) {
        self.selected_room = room;
    }

    /// Run a small back-pagination (expect a batch of 20 events, continue until
    /// we get 10 timeline items or hit the timeline start).
    pub fn back_paginate(&mut self) {
        let Some(sdk_timeline) = self.selected_room.as_deref().and_then(|room_id| {
            self.timelines.lock().get(room_id).map(|timeline| timeline.timeline.clone())
        }) else {
            self.status_handle.set_message("missing timeline for room".to_owned());
            return;
        };

        let mut pagination = self.current_pagination.lock();

        // Cancel the previous back-pagination, if any.
        if let Some(prev) = pagination.take() {
            prev.abort();
        }

        let status_handle = self.status_handle.clone();

        // Start a new one, request batches of 20 events, stop after 10 timeline items
        // have been added.
        *pagination = Some(spawn(async move {
            if let Err(err) = sdk_timeline.paginate_backwards(20).await {
                status_handle.set_message(format!("Error during backpagination: {err}"));
            }
        }));
    }

    pub async fn toggle_reaction_to_latest_msg(&mut self) {
        let selected = self.selected_room.as_deref();

        if let Some((sdk_timeline, items)) = selected.and_then(|room_id| {
            self.timelines
                .lock()
                .get(room_id)
                .map(|timeline| (timeline.timeline.clone(), timeline.items.clone()))
        }) {
            // Look for the latest (most recent) room message.
            let item_id = {
                let items = items.lock();
                items.iter().rev().find_map(|it| {
                    it.as_event()
                        .and_then(|ev| ev.content().as_message().is_some().then(|| ev.identifier()))
                })
            };

            // If found, send a reaction.
            if let Some(item_id) = item_id {
                match sdk_timeline.toggle_reaction(&item_id, "🥰").await {
                    Ok(_) => {
                        self.status_handle.set_message("reaction sent!".to_owned());
                    }
                    Err(err) => {
                        self.status_handle.set_message(format!("error when reacting: {err}"))
                    }
                }
            } else {
                self.status_handle.set_message("no item to react to".to_owned());
            }
        } else {
            self.status_handle.set_message("missing timeline for room".to_owned());
        };
    }

    pub async fn send_message(&self, message: String) -> Result<()> {
        if let Some(sdk_timeline) = self.selected_room.as_deref().and_then(|room_id| {
            self.timelines.lock().get(room_id).map(|timeline| timeline.timeline.clone())
        }) {
            sdk_timeline.send(RoomMessageEventContent::text_plain(message).into()).await?;
        } else {
            self.status_handle.set_message("missing timeline for room".to_owned());
        };

        Ok(())
    }

    /// Mark the currently selected room as read.
    pub async fn mark_as_read(&mut self) {
        let selected = self.selected_room.as_deref();

        let Some(room) = selected.and_then(|room_id| self.ui_rooms.lock().get(room_id).cloned())
        else {
            self.status_handle.set_message("missing room or nothing to show".to_owned());
            return;
        };

        // Mark as read!
        match room.timeline().unwrap().mark_as_read(ReceiptType::Read).await {
            Ok(did) => {
                self.status_handle.set_message(format!(
                    "did {}send a read receipt!",
                    if did { "" } else { "not " }
                ));
            }
            Err(err) => {
                self.status_handle
                    .set_message(format!("error when marking a room as read: {err}",));
            }
        }
    }
}

impl Widget for &mut RoomView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        // Create a space for the header, timeline, and input area.
        let vertical =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]);
        let [header_area, timeline_area, input_area] = vertical.areas(area);

        let header_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room view")
            .title_alignment(Alignment::Center);

        let timeline_block = Block::default()
            .border_set(symbols::border::THICK)
            .bg(NORMAL_ROW_COLOR)
            .padding(Padding::horizontal(1));

        // Let's render the backgrounds for the header and the timeline.
        header_block.render(header_area, buf);
        timeline_block.render(timeline_area, buf);

        // Helper to render some string as a paragraph.
        let render_paragraph = |buf: &mut Buffer, content: String| {
            Paragraph::new(content)
                .fg(TEXT_COLOR)
                .wrap(Wrap { trim: false })
                .render(timeline_area, buf);
        };

        if let Some(room_id) = self.selected_room.as_deref() {
            if let Some(items) =
                self.timelines.lock().get(room_id).map(|timeline| timeline.items.clone())
            {
                let items = items.lock();
                let mut timeline = TimelineView::new(items.deref());
                timeline.render(timeline_area, buf);
            } else {
                render_paragraph(buf, "(room's timeline disappeared)".to_owned())
            };

            let rooms = self.ui_rooms.lock();
            let mut room = rooms.get(room_id);

            self.input.render(input_area, buf, &mut room);

            match &mut self.mode {
                Mode::Normal => {}
                Mode::Details { view } => {
                    let details_area = area.inner(Margin::new(0, 1));
                    Clear.render(details_area, buf);

                    view.render(details_area, buf, &mut room);
                }
            }
        } else {
            render_paragraph(buf, "Nothing to see here...".to_owned())
        };
    }
}
