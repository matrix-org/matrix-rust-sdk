use std::{ops::Deref, sync::Arc};

use color_eyre::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use input::MessageOrCommand;
use invited_room::InvitedRoomView;
use matrix_sdk::{
    locks::Mutex,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::room::message::RoomMessageEventContent, OwnedRoomId, OwnedUserId,
    },
    RoomState,
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
mod invited_room;
mod timeline;

const DEFAULT_TILING_DIRECTION: Direction = Direction::Horizontal;

enum Mode {
    Normal { invited_room_view: Option<InvitedRoomView> },
    Details { tiling_direction: Direction, view: RoomDetails },
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
            mode: Mode::Normal { invited_room_view: None },
            input: Input::new(),
        }
    }

    pub async fn handle_event(&mut self, event: Event) {
        use KeyCode::*;

        match &mut self.mode {
            Mode::Normal { invited_room_view } => {
                if let Some(view) = invited_room_view {
                    view.handle_event(event);
                } else if let Event::Key(key) = event {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, Enter) => {
                            if !self.input.is_empty() {
                                let message_or_command = self.input.get_input();

                                match message_or_command {
                                    Ok(MessageOrCommand::Message(message)) => {
                                        self.send_message(message).await
                                    }
                                    Ok(MessageOrCommand::Command(command)) => {
                                        self.handle_command(command).await
                                    }
                                    Err(e) => {
                                        self.status_handle.set_message(e.render().to_string());
                                        self.input.clear();
                                    }
                                }
                            }
                        }

                        (KeyModifiers::CONTROL, Char('l')) => {
                            self.toggle_reaction_to_latest_msg().await
                        }

                        (KeyModifiers::NONE, PageUp) => self.back_paginate(),

                        (KeyModifiers::ALT, Char('e')) => {
                            if self.selected_room.is_some() {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_events_as_selected(),
                                }
                            }
                        }

                        (KeyModifiers::ALT, Char('r')) => {
                            if self.selected_room.is_some() {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_receipts_as_selected(),
                                }
                            }
                        }

                        (KeyModifiers::ALT, Char('l')) => {
                            if self.selected_room.is_some() {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_chunks_as_selected(),
                                }
                            }
                        }

                        _ => self.input.handle_key_press(key),
                    }
                }
            }

            Mode::Details { view, tiling_direction } => {
                if let Event::Key(key) = event {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, PageUp) => self.back_paginate(),

                        (KeyModifiers::ALT, Char('t')) => {
                            let new_layout = match tiling_direction {
                                Direction::Horizontal => Direction::Vertical,
                                Direction::Vertical => Direction::Horizontal,
                            };

                            *tiling_direction = new_layout;
                        }

                        (KeyModifiers::ALT, Char('e')) => {
                            self.mode = Mode::Details {
                                tiling_direction: *tiling_direction,
                                view: RoomDetails::with_events_as_selected(),
                            }
                        }

                        (KeyModifiers::ALT, Char('r')) => {
                            self.mode = Mode::Details {
                                tiling_direction: *tiling_direction,
                                view: RoomDetails::with_receipts_as_selected(),
                            }
                        }

                        (KeyModifiers::ALT, Char('l')) => {
                            self.mode = Mode::Details {
                                tiling_direction: *tiling_direction,
                                view: RoomDetails::with_chunks_as_selected(),
                            }
                        }

                        _ => match view.handle_key_press(key) {
                            ShouldExit::No => {}
                            ShouldExit::OnlySubScreen => {}
                            ShouldExit::Yes => self.mode = Mode::Normal { invited_room_view: None },
                        },
                    }
                }
            }
        }
    }

    pub fn set_selected_room(&mut self, room: Option<OwnedRoomId>) {
        if let Some(room_id) = room.as_deref() {
            let rooms = self.ui_rooms.lock();
            let maybe_room = rooms.get(room_id);

            if let Some(room) = maybe_room {
                if matches!(room.state(), RoomState::Invited) {
                    let room = room.clone();
                    let view = InvitedRoomView::new(room);
                    self.mode = Mode::Normal { invited_room_view: Some(view) }
                } else {
                    match &mut self.mode {
                        Mode::Normal { invited_room_view } => {
                            invited_room_view.take();
                        }
                        Mode::Details { .. } => {}
                    }
                }
            }
        }

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

    async fn invite_member(&mut self, user_id: OwnedUserId) {
        let Some(room) = self
            .selected_room
            .as_deref()
            .and_then(|room_id| self.ui_rooms.lock().get(room_id).cloned())
        else {
            self.status_handle
                .set_message(format!("Coulnd't find the room object to invite {user_id}"));
            return;
        };

        match room.invite_user_by_id(&user_id).await {
            Ok(_) => {
                self.status_handle
                    .set_message(format!("Successfully invited {user_id} to the room"));
                self.input.clear();
            }
            Err(e) => {
                self.status_handle
                    .set_message(format!("Failed to invite {user_id} to the room: {e:?}"));
            }
        }
    }

    async fn handle_command(&mut self, command: input::Command) {
        match command {
            input::Command::Invite { user_id } => self.invite_member(user_id).await,
        }
    }

    async fn send_message(&mut self, message: String) {
        match self.send_message_impl(message).await {
            Ok(_) => {
                self.input.clear();
            }
            Err(err) => {
                self.status_handle.set_message(format!("error when sending event: {err}"));
            }
        }
    }

    async fn send_message_impl(&self, message: String) -> Result<()> {
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

    fn update(&mut self) {
        match &mut self.mode {
            Mode::Normal { invited_room_view } => {
                if invited_room_view.as_ref().is_some_and(|view| view.should_switch()) {
                    self.mode = Mode::Normal { invited_room_view: None };
                }
            }
            Mode::Details { .. } => {}
        }
    }
}

impl Widget for &mut RoomView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        self.update();

        // Create a space for the header, timeline, and input area.
        let vertical =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]);
        let [header_area, middle_area, input_area] = vertical.areas(area);

        let header_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room view")
            .title_alignment(Alignment::Center);

        let middle_block = Block::default()
            .border_set(symbols::border::THICK)
            .bg(NORMAL_ROW_COLOR)
            .padding(Padding::horizontal(1));

        // Let's render the backgrounds for the header and the timeline.
        header_block.render(header_area, buf);
        middle_block.render(middle_area, buf);

        // Helper to render some string as a paragraph.
        let render_paragraph = |buf: &mut Buffer, content: String| {
            Paragraph::new(content)
                .fg(TEXT_COLOR)
                .wrap(Wrap { trim: false })
                .render(middle_area, buf);
        };

        if let Some(room_id) = self.selected_room.as_deref() {
            let rooms = self.ui_rooms.lock();
            let mut maybe_room = rooms.get(room_id);

            let timeline_area = match &mut self.mode {
                Mode::Normal { invited_room_view } => {
                    if let Some(view) = invited_room_view {
                        view.render(middle_area, buf);

                        None
                    } else {
                        self.input.render(input_area, buf, &mut maybe_room);
                        Some(middle_area)
                    }
                }
                Mode::Details { tiling_direction, view } => {
                    let vertical = Layout::new(
                        *tiling_direction,
                        [Constraint::Percentage(50), Constraint::Percentage(50)],
                    );
                    let [timeline_area, details_area] = vertical.areas(middle_area);
                    Clear.render(details_area, buf);

                    view.render(details_area, buf, &mut maybe_room);

                    Some(timeline_area)
                }
            };

            if let Some(items) =
                self.timelines.lock().get(room_id).map(|timeline| timeline.items.clone())
            {
                if let Some(timeline_area) = timeline_area {
                    let items = items.lock();
                    let mut timeline = TimelineView::new(items.deref());

                    timeline.render(timeline_area, buf);
                }
            }
        } else {
            render_paragraph(buf, "Nothing to see here...".to_owned())
        };
    }
}
