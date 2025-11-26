use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use imbl::Vector;
use input::MessageOrCommand;
use invited_room::InvitedRoomView;
use matrix_sdk::{
    Client, Room, RoomState,
    locks::Mutex,
    ruma::{
        OwnedEventId, OwnedRoomId, RoomId, UserId,
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::room::message::RoomMessageEventContent,
    },
};
use matrix_sdk_ui::{
    Timeline,
    timeline::{TimelineBuilder, TimelineFocus, TimelineItem, TimelineReadReceiptTracking},
};
use ratatui::{prelude::*, widgets::*};
use tokio::{spawn, sync::OnceCell, task::JoinHandle};
use tracing::info;

use self::{details::RoomDetails, input::Input, timeline::TimelineView};
use super::status::StatusHandle;
use crate::{
    HEADER_BG, NORMAL_ROW_COLOR, TEXT_COLOR, Timelines,
    widgets::{recovery::ShouldExit, room_view::timeline::TimelineListState},
};

mod details;
mod input;
mod invited_room;
mod timeline;

const DEFAULT_TILING_DIRECTION: Direction = Direction::Horizontal;

pub struct DetailsState<'a> {
    selected_room: Option<&'a Room>,
    selected_item: Option<Arc<TimelineItem>>,
}

enum Mode {
    Normal { invited_room_view: Option<InvitedRoomView> },
    Details { tiling_direction: Direction, view: RoomDetails },
}

enum TimelineKind {
    Room {
        room: Option<OwnedRoomId>,
    },

    Thread {
        room: OwnedRoomId,
        thread_root: OwnedEventId,
        /// The threaded-focused timeline for this thread.
        timeline: Arc<OnceCell<Arc<Timeline>>>,
        /// Items in the thread timeline (to avoid recomputing them every single
        /// time).
        items: Arc<Mutex<Vector<Arc<TimelineItem>>>>,
        /// Task listening to updates from the threaded timeline, to maintain
        /// the `items` field over time.
        task: JoinHandle<()>,
    },
}

pub struct RoomView {
    client: Client,

    /// Timelines data structures for each room.
    timelines: Timelines,

    status_handle: StatusHandle,

    current_pagination: Arc<Mutex<Option<JoinHandle<()>>>>,

    mode: Mode,
    kind: TimelineKind,

    timeline_list: TimelineListState,

    input: Input,
}

impl RoomView {
    pub fn new(client: Client, timelines: Timelines, status_handle: StatusHandle) -> Self {
        Self {
            client,
            timelines,
            status_handle,
            current_pagination: Default::default(),
            mode: Mode::Normal { invited_room_view: None },
            kind: TimelineKind::Room { room: None },
            input: Input::new(),
            timeline_list: TimelineListState::default(),
        }
    }

    fn switch_to_room_timeline(&mut self, room: Option<OwnedRoomId>) {
        match &mut self.kind {
            TimelineKind::Room { room: prev_room } => {
                self.kind = TimelineKind::Room { room: room.or(prev_room.take()) };
            }
            TimelineKind::Thread { task, room, .. } => {
                // If we were in a thread, abort the task.
                task.abort();
                self.kind = TimelineKind::Room { room: Some(room.clone()) };
            }
        }
    }

    fn switch_to_thread_timeline(&mut self) {
        let Some(room) = self.room() else {
            return;
        };

        let Some(timeline_list_nth) = self.timeline_list.selected() else {
            return;
        };

        let Some(items) = self.get_selected_timeline_items() else {
            self.status_handle.set_message("missing timeline for room".to_owned());
            return;
        };

        let Some(root_event) = items.get(timeline_list_nth).and_then(|item| item.as_event()) else {
            self.status_handle.set_message("no event associated to this timeline item".to_owned());
            return;
        };

        if root_event.content().as_message().is_none() {
            self.status_handle.set_message("this event can't be a thread start!".to_owned());
            return;
        }

        let Some(root_event_id) = root_event.event_id().map(ToOwned::to_owned) else {
            self.status_handle.set_message("can't open thread on a local echo".to_owned());
            return;
        };

        info!("Opening thread view for event {root_event_id} in room {}", room.room_id());

        let thread_timeline = Arc::new(OnceCell::new());
        let items = Arc::new(Mutex::new(Default::default()));

        let i = items.clone();
        let t = thread_timeline.clone();
        let root = root_event_id;
        let cloned_root = root.clone();
        let r = room.clone();
        let task = spawn(async move {
            let timeline = TimelineBuilder::new(&r)
                .with_focus(TimelineFocus::Thread { root_event_id: cloned_root })
                .track_read_marker_and_receipts(TimelineReadReceiptTracking::AllEvents)
                .build()
                .await
                .unwrap();

            let items = i;
            let (initial_items, mut stream) = timeline.subscribe().await;

            t.set(Arc::new(timeline)).unwrap();
            *items.lock() = initial_items;

            while let Some(diffs) = stream.next().await {
                let mut items = items.lock();
                for diff in diffs {
                    diff.apply(&mut items);
                }
            }
        });

        self.timeline_list.unselect();

        self.kind = TimelineKind::Thread {
            thread_root: root,
            room: room.room_id().to_owned(),
            timeline: thread_timeline,
            items,
            task,
        };
    }

    fn room_id(&self) -> Option<&RoomId> {
        match &self.kind {
            TimelineKind::Room { room } => room.as_deref(),
            TimelineKind::Thread { room, .. } => Some(room),
        }
    }

    /// Get currently focused [`Room`]
    pub fn room(&self) -> Option<Room> {
        self.room_id().and_then(|room_id| self.client.get_room(room_id))
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

                        // Pressing Escape on a threaded timeline will get back to the room
                        // timeline.
                        (KeyModifiers::NONE, Esc)
                            if matches!(self.kind, TimelineKind::Thread { .. }) =>
                        {
                            self.switch_to_room_timeline(None);
                        }

                        // Pressing 'Alt+s' on a threaded timeline will print the current
                        // subscription status.
                        (KeyModifiers::ALT, Char('s')) => {
                            self.print_thread_subscription_status().await;
                        }

                        (KeyModifiers::CONTROL, Char('l')) => {
                            self.toggle_reaction_to_latest_msg().await
                        }

                        (KeyModifiers::NONE, PageUp) => self.back_paginate(),

                        (KeyModifiers::ALT, Char('e')) => {
                            if let TimelineKind::Room { room: Some(_) } = self.kind {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_events_as_selected(),
                                }
                            }
                        }

                        (KeyModifiers::ALT, Char('r')) => {
                            if let TimelineKind::Room { room: Some(_) } = self.kind {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_receipts_as_selected(),
                                }
                            }
                        }

                        (KeyModifiers::ALT, Char('l')) => {
                            if let TimelineKind::Room { room: Some(_) } = self.kind {
                                self.mode = Mode::Details {
                                    tiling_direction: DEFAULT_TILING_DIRECTION,
                                    view: RoomDetails::with_chunks_as_selected(),
                                }
                            }
                        }

                        (_, Down) | (KeyModifiers::CONTROL, Char('n')) => {
                            self.timeline_list.select_next()
                        }
                        (_, Up) | (KeyModifiers::CONTROL, Char('p')) => {
                            self.timeline_list.select_previous()
                        }
                        (_, Esc) => self.timeline_list.unselect(),

                        (KeyModifiers::CONTROL, Char('t'))
                            if matches!(self.kind, TimelineKind::Room { .. }) =>
                        {
                            self.switch_to_thread_timeline();
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

                        (_, Down) | (KeyModifiers::CONTROL, Char('n')) => {
                            self.timeline_list.select_next()
                        }

                        (_, Up) | (KeyModifiers::CONTROL, Char('p')) => {
                            self.timeline_list.select_previous()
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

    pub fn set_selected_room(&mut self, room_id: Option<OwnedRoomId>) {
        if let Some(room_id) = room_id.as_deref() {
            let maybe_room = self.client.get_room(room_id);

            if let Some(room) = maybe_room {
                self.switch_to_room_timeline(Some(room_id.to_owned()));

                if matches!(room.state(), RoomState::Invited) {
                    let view = InvitedRoomView::new(room);
                    self.mode = Mode::Normal { invited_room_view: Some(view) };
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

        self.timeline_list = TimelineListState::default();
    }

    fn get_selected_timeline(&self) -> Option<Arc<Timeline>> {
        match &self.kind {
            TimelineKind::Room { room } => room
                .as_deref()
                .and_then(|room_id| Some(self.timelines.lock().get(room_id)?.timeline.clone())),
            TimelineKind::Thread { timeline, .. } => timeline.get().cloned(),
        }
    }

    fn get_selected_timeline_items(&self) -> Option<Vector<Arc<TimelineItem>>> {
        match &self.kind {
            TimelineKind::Room { room } => room
                .as_deref()
                .and_then(|room_id| Some(self.timelines.lock().get(room_id)?.items.lock().clone())),
            TimelineKind::Thread { items, .. } => Some(items.lock().clone()),
        }
    }

    /// Run a small back-pagination (expect a batch of 20 events, continue until
    /// we get 10 timeline items or hit the timeline start).
    pub fn back_paginate(&mut self) {
        let Some(sdk_timeline) = self.get_selected_timeline() else {
            self.status_handle.set_message("missing timeline for room".to_owned());
            return;
        };

        let mut pagination = self.current_pagination.lock();

        // Cancel the previous back-pagination, if any.
        if let Some(prev) = pagination.take() {
            prev.abort();
        }

        let status_handle = self.status_handle.clone();

        // Request to back-paginate 5 events.
        *pagination = Some(spawn(async move {
            if let Err(err) = sdk_timeline.paginate_backwards(5).await {
                status_handle.set_message(format!("Error during backpagination: {err}"));
            }
        }));
    }

    pub async fn toggle_reaction_to_latest_msg(&mut self) {
        let Some((sdk_timeline, items)) =
            self.get_selected_timeline().zip(self.get_selected_timeline_items())
        else {
            self.status_handle.set_message("missing timeline for room".to_owned());
            return;
        };

        // Look for the latest (most recent) room message.
        let Some(item_id) = items.iter().rev().find_map(|it| {
            let event_item = it.as_event()?;
            event_item.content().as_message()?;
            Some(event_item.identifier())
        }) else {
            self.status_handle.set_message("no item to react to".to_owned());
            return;
        };

        // If found, send a reaction.
        match sdk_timeline.toggle_reaction(&item_id, "🥰").await {
            Ok(_) => {
                self.status_handle.set_message("reaction sent!".to_owned());
            }
            Err(err) => self.status_handle.set_message(format!("error when reacting: {err}")),
        }
    }

    /// Attempt to find the currently selected room and pass it to the async
    /// callback.
    async fn call_with_room(&self, function: impl AsyncFnOnce(Room, &StatusHandle)) {
        if let Some(room) = self.room() {
            function(room, &self.status_handle).await
        } else {
            self.status_handle
                .set_message("Couldn't find a room selected room to perform an action".to_owned());
        }
    }

    async fn invite_member(&mut self, user_id: &str) {
        self.call_with_room(async move |room, status_handle| {
            let user_id = match UserId::parse_with_server_name(
                user_id,
                room.client().user_id().unwrap().server_name(),
            ) {
                Ok(user_id) => user_id,
                Err(e) => {
                    status_handle
                        .set_message(format!("Failed to parse {user_id} as a user ID: {e:?}"));
                    return;
                }
            };

            match room.invite_user_by_id(&user_id).await {
                Ok(_) => {
                    status_handle
                        .set_message(format!("Successfully invited {user_id} to the room"));
                }
                Err(e) => {
                    status_handle
                        .set_message(format!("Failed to invite {user_id} to the room: {e:?}"));
                }
            }
        })
        .await;

        self.input.clear();
    }

    async fn leave_room(&mut self) {
        self.call_with_room(async |room, status_handle| {
            let _ = room.leave().await.inspect_err(|e| {
                status_handle.set_message(format!("Couldn't leave the room {e:?}"))
            });
        })
        .await;

        self.input.clear();
    }

    async fn subscribe_thread(&mut self) {
        if let TimelineKind::Thread { thread_root, .. } = &self.kind {
            self.call_with_room(async |room, status_handle| {
                if let Err(err) = room.subscribe_thread(thread_root.clone(), None).await {
                    status_handle.set_message(format!("error when subscribing to a thread: {err}"));
                } else {
                    status_handle.set_message("Subscribed to thread!".to_owned());
                }
            })
            .await;

            self.input.clear();
        }
    }

    async fn unsubscribe_thread(&mut self) {
        if let TimelineKind::Thread { thread_root, .. } = &self.kind {
            self.call_with_room(async |room, status_handle| {
                if let Err(err) = room.unsubscribe_thread(thread_root.clone()).await {
                    status_handle
                        .set_message(format!("error when unsubscribing to a thread: {err}"));
                } else {
                    status_handle.set_message("Unsubscribed from thread!".to_owned());
                }
            })
            .await;

            self.input.clear();
        }
    }

    async fn print_thread_subscription_status(&mut self) {
        if let TimelineKind::Thread { thread_root, .. } = &self.kind {
            self.call_with_room(async |room, status_handle| {
                match room.load_or_fetch_thread_subscription(thread_root).await {
                    Ok(Some(subscription)) => {
                        status_handle.set_message(format!(
                            "Thread subscription status: {}",
                            if subscription.automatic {
                                "subscribed (automatic)"
                            } else {
                                "subscribed (manual)"
                            }
                        ));
                    }
                    Ok(None) => {
                        status_handle
                            .set_message("Thread is not subscribed or does not exist".to_owned());
                    }
                    Err(err) => {
                        status_handle
                            .set_message(format!("Error getting thread subscription: {err}"));
                    }
                }
            })
            .await;
        }
    }

    async fn handle_command(&mut self, command: input::Command) {
        match command {
            input::Command::Invite { user_id } => self.invite_member(&user_id).await,
            input::Command::Leave => self.leave_room().await,
            input::Command::Subscribe => self.subscribe_thread().await,
            input::Command::Unsubscribe => self.unsubscribe_thread().await,
        }
    }

    async fn send_message(&mut self, message: String) {
        if let Some(sdk_timeline) = self.get_selected_timeline() {
            match sdk_timeline.send(RoomMessageEventContent::text_plain(message).into()).await {
                Ok(_) => {
                    self.input.clear();
                }
                Err(err) => {
                    self.status_handle.set_message(format!("error when sending event: {err}"));
                }
            }
        } else {
            self.status_handle.set_message("missing timeline for room".to_owned());
        }
    }

    /// Mark the currently selected room as read.
    pub async fn mark_as_read(&mut self) {
        let Some(sdk_timeline) = self.get_selected_timeline() else {
            self.status_handle.set_message("missing timeline for room".to_owned());
            return;
        };

        match sdk_timeline.mark_as_read(ReceiptType::Read).await {
            Ok(did) => {
                self.status_handle.set_message(format!(
                    "did {}send a read receipt!",
                    if did { "" } else { "not " }
                ));
            }
            Err(err) => {
                self.status_handle.set_message(format!("error when marking a room as read: {err}"));
            }
        }
    }

    fn get_selected_event(&self) -> Option<Arc<TimelineItem>> {
        let selected = self.timeline_list.selected()?;
        let items = self.get_selected_timeline_items()?;
        items.get(selected).cloned()
    }

    fn update(&mut self) {
        match &mut self.mode {
            Mode::Normal { invited_room_view } => {
                if let Some(view) = invited_room_view
                    && view.should_switch()
                {
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

        let is_thread_view = matches!(self.kind, TimelineKind::Thread { .. });
        let title = if is_thread_view { "Thread view" } else { "Room view" };

        let header_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title(title)
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

        if let Some(room_id) = self.room_id() {
            let maybe_room = self.client.get_room(room_id);
            let mut maybe_room = maybe_room.as_ref();

            let selected_event = self.get_selected_event();

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

                    let mut state =
                        DetailsState { selected_room: maybe_room, selected_item: selected_event };

                    view.render(details_area, buf, &mut state);

                    Some(timeline_area)
                }
            };

            if let Some(timeline_area) = timeline_area
                && let Some(items) = self.get_selected_timeline_items()
            {
                let is_thread = matches!(self.kind, TimelineKind::Thread { .. });
                let mut timeline = TimelineView::new(&items, is_thread);
                timeline.render(timeline_area, buf, &mut self.timeline_list);
            }
        } else {
            render_paragraph(buf, "Nothing to see here...".to_owned())
        }
    }
}
