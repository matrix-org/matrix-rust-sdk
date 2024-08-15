use std::{
    collections::HashMap,
    env,
    io::{self, stdout, Write},
    path::PathBuf,
    process::exit,
    sync::{Arc, Mutex},
    time::Duration,
};

use color_eyre::config::HookBuilder;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures_util::{pin_mut, StreamExt as _};
use imbl::Vector;
use matrix_sdk::{
    config::StoreConfig,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    matrix_auth::MatrixSession,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::room::message::{MessageType, RoomMessageEventContent},
        MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId,
    },
    AuthSession, Client, ServerName, SqliteCryptoStore, SqliteStateStore,
};
use matrix_sdk_ui::{
    room_list_service::{self, filters::new_filter_non_left},
    sync_service::{self, SyncService},
    timeline::{TimelineItem, TimelineItemContent, TimelineItemKind, VirtualTimelineItem},
    Timeline as SdkTimeline,
};
use ratatui::{prelude::*, style::palette::tailwind, widgets::*};
use tokio::{runtime::Handle, spawn, task::JoinHandle};
use tracing::error;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

const HEADER_BG: Color = tailwind::BLUE.c950;
const NORMAL_ROW_COLOR: Color = tailwind::SLATE.c950;
const ALT_ROW_COLOR: Color = tailwind::SLATE.c900;
const SELECTED_STYLE_FG: Color = tailwind::BLUE.c300;
const TEXT_COLOR: Color = tailwind::SLATE.c200;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(tracing_appender::rolling::hourly("/tmp/", "logs-"));

    tracing_subscriber::registry()
        .with(EnvFilter::new(env::var("RUST_LOG").unwrap_or("".into())))
        .with(file_layer)
        .init();

    // Read the server name from the command line.
    let Some(server_name) = env::args().nth(1) else {
        eprintln!("Usage: {} <server_name> <session_path?>", env::args().next().unwrap());
        exit(1)
    };

    let config_path = env::args().nth(2).unwrap_or("/tmp/".to_owned());
    let client = configure_client(server_name, config_path).await?;

    init_error_hooks()?;
    let terminal = init_terminal()?;

    let mut app = App::new(client).await?;

    app.run(terminal).await
}

fn init_error_hooks() -> anyhow::Result<()> {
    let (panic, error) = HookBuilder::default().into_hooks();
    let panic = panic.into_panic_hook();
    let error = error.into_eyre_hook();
    color_eyre::eyre::set_hook(Box::new(move |e| {
        let _ = restore_terminal();
        error(e)
    }))?;
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        panic(info)
    }));
    Ok(())
}

fn init_terminal() -> anyhow::Result<Terminal<impl Backend>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() -> anyhow::Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Default)]
struct StatefulList<T> {
    state: ListState,
    items: Arc<Mutex<Vector<T>>>,
}

#[derive(Default, PartialEq)]
enum DetailsMode {
    ReadReceipts,
    #[default]
    TimelineItems,
    Events,
}

struct Timeline {
    timeline: Arc<SdkTimeline>,
    items: Arc<Mutex<Vector<Arc<TimelineItem>>>>,
    task: JoinHandle<()>,
}

/// Extra room information, like its display name, etc.
#[derive(Clone)]
struct ExtraRoomInfo {
    /// Content of the raw m.room.name event, if available.
    raw_name: Option<String>,

    /// Calculated display name for the room.
    display_name: Option<String>,
}

struct App {
    /// Reference to the main SDK client.
    client: Client,

    /// The sync service used for synchronizing events.
    sync_service: Arc<SyncService>,

    /// Room list service rooms known to the app.
    ui_rooms: Arc<Mutex<HashMap<OwnedRoomId, room_list_service::Room>>>,

    /// Timelines data structures for each room.
    timelines: Arc<Mutex<HashMap<OwnedRoomId, Timeline>>>,

    /// Ratatui's list of room list rooms.
    room_list_rooms: StatefulList<room_list_service::Room>,

    /// Extra information about rooms.
    room_info: Arc<Mutex<HashMap<OwnedRoomId, ExtraRoomInfo>>>,

    /// Task listening to room list service changes, and spawning timelines.
    listen_task: JoinHandle<()>,

    /// Content of the latest status message, if set.
    last_status_message: Arc<Mutex<Option<String>>>,

    /// A task to automatically clear the status message in N seconds, if set.
    clear_status_message: Option<JoinHandle<()>>,

    /// What's shown in the details view, aka the right panel.
    details_mode: DetailsMode,

    /// The current room that's subscribed to in the room list's sliding sync.
    current_room_subscription: Option<room_list_service::Room>,

    current_pagination: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl App {
    async fn new(client: Client) -> anyhow::Result<Self> {
        let sync_service = Arc::new(SyncService::builder(client.clone()).build().await?);

        let rooms = Arc::new(Mutex::new(Vector::<room_list_service::Room>::new()));
        let room_infos: Arc<Mutex<HashMap<OwnedRoomId, ExtraRoomInfo>>> =
            Arc::new(Mutex::new(Default::default()));
        let ui_rooms: Arc<Mutex<HashMap<OwnedRoomId, room_list_service::Room>>> =
            Default::default();
        let timelines = Arc::new(Mutex::new(HashMap::new()));

        let c = client.clone();
        let r = rooms.clone();
        let ri = room_infos.clone();
        let ur = ui_rooms.clone();
        let t = timelines.clone();

        let room_list_service = sync_service.room_list_service();
        let all_rooms = room_list_service.all_rooms().await?;

        let listen_task = spawn(async move {
            let client = c;
            let rooms = r;
            let room_infos = ri;
            let ui_rooms = ur;
            let timelines = t;

            let (stream, entries_controller) = all_rooms
                .entries_with_dynamic_adapters(50_000, client.room_info_notable_update_receiver());
            entries_controller.set_filter(Box::new(new_filter_non_left()));

            pin_mut!(stream);

            while let Some(diffs) = stream.next().await {
                let all_rooms = {
                    // Apply the diffs to the list of room entries.
                    let mut rooms = rooms.lock().unwrap();

                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }

                    // Collect rooms early to release the room entries list lock.
                    (*rooms).clone()
                };

                // Clone the previous set of ui rooms to avoid keeping the ui_rooms lock (which
                // we couldn't do below, because it's a sync lock, and has to be
                // sync b/o rendering; and we'd have to cross await points
                // below).
                let previous_ui_rooms = ui_rooms.lock().unwrap().clone();

                let mut new_ui_rooms = HashMap::new();
                let mut new_timelines = Vec::new();

                // Initialize all the new rooms.
                for ui_room in all_rooms
                    .into_iter()
                    .filter(|room| !previous_ui_rooms.contains_key(room.room_id()))
                {
                    // Initialize the timeline.
                    let builder = match ui_room.default_room_timeline_builder().await {
                        Ok(builder) => builder,
                        Err(err) => {
                            error!("error when getting default timeline builder: {err}");
                            continue;
                        }
                    };

                    if let Err(err) = ui_room.init_timeline_with_builder(builder).await {
                        error!("error when creating default timeline: {err}");
                    }

                    // Save the timeline in the cache.
                    let sdk_timeline = ui_room.timeline().unwrap();
                    let (items, stream) = sdk_timeline.subscribe().await;
                    let items = Arc::new(Mutex::new(items));

                    // Spawn a timeline task that will listen to all the timeline item changes.
                    let i = items.clone();
                    let timeline_task = spawn(async move {
                        pin_mut!(stream);
                        let items = i;
                        while let Some(diff) = stream.next().await {
                            let mut items = items.lock().unwrap();
                            diff.apply(&mut items);
                        }
                    });

                    new_timelines.push((
                        ui_room.room_id().to_owned(),
                        Timeline { timeline: sdk_timeline, items, task: timeline_task },
                    ));

                    // Save the room list service room in the cache.
                    new_ui_rooms.insert(ui_room.room_id().to_owned(), ui_room);
                }

                for (room_id, room) in &new_ui_rooms {
                    let raw_name = room.name();
                    let display_name = room.cached_display_name();
                    room_infos
                        .lock()
                        .unwrap()
                        .insert(room_id.to_owned(), ExtraRoomInfo { raw_name, display_name });
                }

                ui_rooms.lock().unwrap().extend(new_ui_rooms);
                timelines.lock().unwrap().extend(new_timelines);
            }
        });

        // This will sync (with encryption) until an error happens or the program is
        // stopped.
        sync_service.start().await;

        Ok(Self {
            sync_service,
            room_list_rooms: StatefulList { state: Default::default(), items: rooms },
            room_info: room_infos,
            client,
            listen_task,
            last_status_message: Default::default(),
            clear_status_message: None,
            ui_rooms,
            details_mode: Default::default(),
            timelines,
            current_room_subscription: None,
            current_pagination: Default::default(),
        })
    }
}

impl App {
    /// Set the current status message (displayed at the bottom), for a few
    /// seconds.
    fn set_status_message(&mut self, status: String) {
        if let Some(handle) = self.clear_status_message.take() {
            // Cancel the previous task to clear the status message.
            handle.abort();
        }

        *self.last_status_message.lock().unwrap() = Some(status);

        let message = self.last_status_message.clone();
        self.clear_status_message = Some(spawn(async move {
            // Clear the status message in 4 seconds.
            tokio::time::sleep(Duration::from_secs(4)).await;

            *message.lock().unwrap() = None;
        }));
    }

    /// Mark the currently selected room as read.
    async fn mark_as_read(&mut self) {
        let Some(room) = self
            .get_selected_room_id(None)
            .and_then(|room_id| self.ui_rooms.lock().unwrap().get(&room_id).cloned())
        else {
            self.set_status_message("missing room or nothing to show".to_owned());
            return;
        };

        // Mark as read!
        match room.timeline().unwrap().mark_as_read(ReceiptType::Read).await {
            Ok(did) => {
                self.set_status_message(format!(
                    "did {}send a read receipt!",
                    if did { "" } else { "not " }
                ));
            }
            Err(err) => {
                self.set_status_message(format!("error when marking a room as read: {err}",));
            }
        }
    }

    /// Run a small back-pagination (expect a batch of 20 events, continue until
    /// we get 10 timeline items or hit the timeline start).
    fn back_paginate(&mut self) {
        let Some(sdk_timeline) = self.get_selected_room_id(None).and_then(|room_id| {
            self.timelines.lock().unwrap().get(&room_id).map(|timeline| timeline.timeline.clone())
        }) else {
            self.set_status_message("missing timeline for room".to_owned());
            return;
        };

        let mut pagination = self.current_pagination.lock().unwrap();

        // Cancel the previous back-pagination, if any.
        if let Some(prev) = pagination.take() {
            prev.abort();
        }

        // Start a new one, request batches of 20 events, stop after 10 timeline items
        // have been added.
        *pagination = Some(spawn(async move {
            if let Err(err) = sdk_timeline.live_paginate_backwards(20).await {
                // TODO: would be nice to be able to set the status
                // message remotely?
                //self.set_status_message(format!(
                //"Error during backpagination: {err}"
                //));
                error!("Error during backpagination: {err}")
            }
        }));
    }

    /// Returns the currently selected room id, if any.
    fn get_selected_room_id(&self, selected: Option<usize>) -> Option<OwnedRoomId> {
        let selected = selected.or_else(|| self.room_list_rooms.state.selected())?;

        self.room_list_rooms
            .items
            .lock()
            .unwrap()
            .get(selected)
            .cloned()
            .map(|room| room.room_id().to_owned())
    }

    fn subscribe_to_selected_room(&mut self, selected: usize) {
        // Cancel the subscription to the previous room, if any.
        self.current_room_subscription.take();

        // Subscribe to the new room.
        if let Some(room) = self
            .get_selected_room_id(Some(selected))
            .and_then(|room_id| self.ui_rooms.lock().unwrap().get(&room_id).cloned())
        {
            self.sync_service.room_list_service().subscribe_to_rooms(&[room.room_id()], None);
            self.current_room_subscription = Some(room);
        }
    }

    async fn render_loop(&mut self, mut terminal: Terminal<impl Backend>) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| f.render_widget(&mut *self, f.size()))?;

            if event::poll(Duration::from_millis(16))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        use KeyCode::*;
                        match key.code {
                            Char('q') | Esc => return Ok(()),

                            Char('j') | Down => {
                                if let Some(i) = self.room_list_rooms.next() {
                                    self.subscribe_to_selected_room(i);
                                }
                            }

                            Char('k') | Up => {
                                if let Some(i) = self.room_list_rooms.previous() {
                                    self.subscribe_to_selected_room(i);
                                }
                            }

                            Char('s') => self.sync_service.start().await,
                            Char('S') => self.sync_service.stop().await?,

                            Char('Q') => {
                                let q = self.client.send_queue();
                                let enabled = q.is_enabled();
                                q.set_enabled(!enabled).await;
                            }

                            Char('M') => {
                                let selected = self.get_selected_room_id(None);

                                if let Some(sdk_timeline) = selected.and_then(|room_id| {
                                    self.timelines
                                        .lock()
                                        .unwrap()
                                        .get(&room_id)
                                        .map(|timeline| timeline.timeline.clone())
                                }) {
                                    match sdk_timeline
                                        .send(
                                            RoomMessageEventContent::text_plain(format!(
                                                "hey {}",
                                                MilliSecondsSinceUnixEpoch::now().get()
                                            ))
                                            .into(),
                                        )
                                        .await
                                    {
                                        Ok(_) => {
                                            self.set_status_message("message sent!".to_owned());
                                        }
                                        Err(err) => {
                                            self.set_status_message(format!(
                                                "error when sending event: {err}"
                                            ));
                                        }
                                    }
                                } else {
                                    self.set_status_message("missing timeline for room".to_owned());
                                };
                            }

                            Char('r') => self.details_mode = DetailsMode::ReadReceipts,
                            Char('t') => self.details_mode = DetailsMode::TimelineItems,
                            Char('e') => self.details_mode = DetailsMode::Events,

                            Char('b') if self.details_mode == DetailsMode::TimelineItems => {
                                self.back_paginate();
                            }

                            Char('m') if self.details_mode == DetailsMode::ReadReceipts => {
                                self.mark_as_read().await
                            }

                            _ => {}
                        }
                    }
                }
            }
        }
    }

    async fn run(&mut self, terminal: Terminal<impl Backend>) -> anyhow::Result<()> {
        self.render_loop(terminal).await?;

        // At this point the user has exited the loop, so shut down the application.
        restore_terminal()?;

        println!("Closing sync service...");

        let s = self.sync_service.clone();
        let wait_for_termination = spawn(async move {
            while let Some(state) = s.state().next().await {
                if !matches!(state, sync_service::State::Running) {
                    break;
                }
            }
        });

        self.sync_service.stop().await?;
        self.listen_task.abort();
        for timeline in self.timelines.lock().unwrap().values() {
            timeline.task.abort();
        }
        wait_for_termination.await.unwrap();

        println!("okthxbye!");
        Ok(())
    }
}

impl Widget for &mut App {
    /// Render the whole app.
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Create a space for header, todo list and the footer.
        let vertical =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)]);
        let [header_area, rest_area, footer_area] = vertical.areas(area);

        // Create two chunks with equal horizontal screen space. One for the list and
        // the other for the info block.
        let horizontal =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]);
        let [lhs, rhs] = horizontal.areas(rest_area);

        self.render_title(header_area, buf);
        self.render_left(lhs, buf);
        self.render_right(rhs, buf);
        self.render_footer(footer_area, buf);
    }
}

impl App {
    /// Render the top square (title of the program).
    fn render_title(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("Multiverse").bold().centered().render(area, buf);
    }

    /// Renders the left part of the screen, that is, the list of rooms.
    fn render_left(&mut self, area: Rect, buf: &mut Buffer) {
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
        let mut room_info = self.room_info.lock().unwrap().clone();

        // Iterate through all elements in the `items` and stylize them.
        let items: Vec<ListItem<'_>> = self
            .room_list_rooms
            .items
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

                    let (raw, display) = if let Some(info) = room_info {
                        (info.raw_name, info.display_name)
                    } else {
                        (None, None)
                    };

                    let room_name = if let Some(n) = display {
                        format!("{n} ({room_id})")
                    } else if let Some(n) = raw {
                        format!("m.room.name:{n} ({room_id})")
                    } else {
                        room_id.to_string()
                    };

                    format!("#{i} {}", room_name)
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

        StatefulWidget::render(items, inner_area, buf, &mut self.room_list_rooms.state);
    }

    /// Render the right part of the screen, showing the details of the current
    /// view.
    fn render_right(&mut self, area: Rect, buf: &mut Buffer) {
        // Split the block into two parts:
        // - outer_block with the title of the block.
        // - inner_block that will contain the actual details.
        let outer_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room view")
            .title_alignment(Alignment::Center);
        let inner_block = Block::default()
            .borders(Borders::NONE)
            .bg(NORMAL_ROW_COLOR)
            .padding(Padding::horizontal(1));

        // This is a similar process to what we did for list. outer_info_area will be
        // used for header inner_info_area will be used for the list info.
        let outer_area = area;
        let inner_area = outer_block.inner(outer_area);

        // We can render the header. Inner area will be rendered later.
        outer_block.render(outer_area, buf);

        // Helper to render some string as a paragraph.
        let render_paragraph = |buf: &mut Buffer, content: String| {
            Paragraph::new(content)
                .block(inner_block.clone())
                .fg(TEXT_COLOR)
                .wrap(Wrap { trim: false })
                .render(inner_area, buf);
        };

        if let Some(room_id) = self.get_selected_room_id(None) {
            match self.details_mode {
                DetailsMode::ReadReceipts => {
                    // In read receipts mode, show the read receipts object as computed
                    // by the client.
                    match self.ui_rooms.lock().unwrap().get(&room_id).cloned() {
                        Some(room) => {
                            let receipts = room.read_receipts();
                            render_paragraph(
                                buf,
                                format!(
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
                                ),
                            )
                        }
                        None => render_paragraph(
                            buf,
                            "(room disappeared in the room list service)".to_owned(),
                        ),
                    }
                }

                DetailsMode::TimelineItems => {
                    if !self.render_timeline(&room_id, inner_block.clone(), inner_area, buf) {
                        render_paragraph(buf, "(room's timeline disappeared)".to_owned())
                    }
                }

                DetailsMode::Events => match self.ui_rooms.lock().unwrap().get(&room_id).cloned() {
                    Some(room) => {
                        let events = tokio::task::block_in_place(|| {
                            Handle::current().block_on(async {
                                let (room_event_cache, _drop_handles) =
                                    room.event_cache().await.unwrap();
                                let (events, _) = room_event_cache.subscribe().await.unwrap();
                                events
                            })
                        });

                        let rendered_events = events
                            .into_iter()
                            .map(|sync_timeline_item| sync_timeline_item.event.json().to_string())
                            .collect::<Vec<_>>()
                            .join("\n\n");

                        render_paragraph(buf, format!("Events:\n\n{rendered_events}"))
                    }

                    None => render_paragraph(
                        buf,
                        "(room disappeared in the room list service)".to_owned(),
                    ),
                },
            }
        } else {
            render_paragraph(buf, "Nothing to see here...".to_owned())
        };
    }

    /// Renders the list of timeline items for the given room.
    fn render_timeline(
        &mut self,
        room_id: &RoomId,
        inner_block: Block<'_>,
        inner_area: Rect,
        buf: &mut Buffer,
    ) -> bool {
        let Some(items) =
            self.timelines.lock().unwrap().get(room_id).map(|timeline| timeline.items.clone())
        else {
            return false;
        };

        let items = items.lock().unwrap();
        let mut content = Vec::new();

        for item in items.iter() {
            match item.kind() {
                TimelineItemKind::Event(ev) => {
                    let sender = ev.sender();

                    match ev.content() {
                        TimelineItemContent::Message(message) => {
                            if let MessageType::Text(text) = message.msgtype() {
                                content.push(format!("{}: {}", sender, text.body))
                            }
                        }

                        TimelineItemContent::RedactedMessage => {
                            content.push(format!("{}: -- redacted --", sender))
                        }
                        TimelineItemContent::UnableToDecrypt(_) => {
                            content.push(format!("{}: (UTD)", sender))
                        }
                        TimelineItemContent::Sticker(_)
                        | TimelineItemContent::MembershipChange(_)
                        | TimelineItemContent::ProfileChange(_)
                        | TimelineItemContent::OtherState(_)
                        | TimelineItemContent::FailedToParseMessageLike { .. }
                        | TimelineItemContent::FailedToParseState { .. }
                        | TimelineItemContent::Poll(_)
                        | TimelineItemContent::CallInvite
                        | TimelineItemContent::CallNotify => {
                            continue;
                        }
                    }
                }

                TimelineItemKind::Virtual(virt) => match virt {
                    VirtualTimelineItem::DayDivider(unix_ts) => {
                        content.push(format!("Date: {unix_ts:?}"));
                    }
                    VirtualTimelineItem::ReadMarker => {
                        content.push("Read marker".to_owned());
                    }
                },
            }
        }

        let list_items = content
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };
                let line = Line::styled(line, TEXT_COLOR);
                ListItem::new(line).bg(bg_color)
            })
            .collect::<Vec<_>>();

        let list = List::new(list_items)
            .block(inner_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
                    .fg(SELECTED_STYLE_FG),
            )
            .highlight_symbol(">")
            .highlight_spacing(HighlightSpacing::Always);

        let mut dummy_list_state = ListState::default();
        StatefulWidget::render(list, inner_area, buf, &mut dummy_list_state);
        true
    }

    /// Render the bottom part of the screen, with a status message if one is
    /// set, or a default help message otherwise.
    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let content = if let Some(status_message) = self.last_status_message.lock().unwrap().clone()
        {
            status_message
        } else {
            match self.details_mode {
                DetailsMode::ReadReceipts => {
                    "\nUse j/k to move, s/S to start/stop the sync service, m to mark as read, t to show the timeline, e to show events.".to_owned()
                }
                DetailsMode::TimelineItems => {
                    "\nUse j/k to move, s/S to start/stop the sync service, r to show read receipts, e to show events, Q to enable/disable the send queue, M to send a message.".to_owned()
                }
                DetailsMode::Events => {
                    "\nUse j/k to move, s/S to start/stop the sync service, r to show read receipts, t to show the timeline".to_owned()
                }
            }
        };
        Paragraph::new(content).centered().render(area, buf);
    }
}

impl<T> StatefulList<T> {
    /// Focus the list on the next item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    fn next(&mut self) -> Option<usize> {
        let num_items = self.items.lock().unwrap().len();

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
    fn previous(&mut self) -> Option<usize> {
        let num_items = self.items.lock().unwrap().len();

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

/// Configure the client so it's ready for sync'ing.
///
/// Will log in or reuse a previous session.
async fn configure_client(server_name: String, config_path: String) -> anyhow::Result<Client> {
    let server_name = ServerName::parse(&server_name)?;

    let config_path = PathBuf::from(config_path);
    let mut client_builder = Client::builder()
        .store_config(
            StoreConfig::default()
                .crypto_store(
                    SqliteCryptoStore::open(config_path.join("crypto.sqlite"), None).await?,
                )
                .state_store(SqliteStateStore::open(config_path.join("state.sqlite"), None).await?),
        )
        .server_name(&server_name)
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            auto_enable_backups: true,
        });

    if let Ok(proxy_url) = env::var("PROXY") {
        client_builder = client_builder.proxy(proxy_url).disable_ssl_verification();
    }

    let client = client_builder.build().await?;

    // Try reading a session, otherwise create a new one.
    let session_path = config_path.join("session.json");
    if let Ok(serialized) = std::fs::read_to_string(&session_path) {
        let session: MatrixSession = serde_json::from_str(&serialized)?;
        client.restore_session(session).await?;
        println!("restored session");
    } else {
        login_with_password(&client).await?;
        println!("new login");

        // Immediately save the session to disk.
        if let Some(session) = client.session() {
            let AuthSession::Matrix(session) = session else { panic!("unexpected oidc session") };
            let serialized = serde_json::to_string(&session)?;
            std::fs::write(session_path, serialized)?;
            println!("saved session");
        }
    }

    Ok(client)
}

/// Asks the user of a username and password, and try to login using the matrix
/// auth with those.
async fn login_with_password(client: &Client) -> anyhow::Result<()> {
    println!("Logging in with username and password…");

    loop {
        print!("\nUsername: ");
        stdout().flush().expect("Unable to write to stdout");
        let mut username = String::new();
        io::stdin().read_line(&mut username).expect("Unable to read user input");
        username = username.trim().to_owned();

        let password = rpassword::prompt_password("Password.")?;

        match client.matrix_auth().login_username(&username, password.trim()).await {
            Ok(_) => {
                println!("Logged in as {username}");
                break;
            }
            Err(error) => {
                println!("Error logging in: {error}");
                println!("Please try again\n");
            }
        }
    }

    Ok(())
}
