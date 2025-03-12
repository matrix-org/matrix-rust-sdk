use std::{
    collections::HashMap,
    io::{self, stdout, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::{pin_mut, StreamExt as _};
use imbl::Vector;
use layout::Flex;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::StoreConfig,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    reqwest::Url,
    ruma::{
        events::room::message::{MessageType, RoomMessageEventContent},
        MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId,
    },
    AuthSession, Client, SqliteCryptoStore, SqliteEventCacheStore, SqliteStateStore,
};
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_ui::{
    room_list_service::{self, filters::new_filter_non_left},
    sync_service::SyncService,
    timeline::{
        MsgLikeContent, MsgLikeKind, TimelineItem, TimelineItemContent, TimelineItemKind,
        VirtualTimelineItem,
    },
    Timeline as SdkTimeline,
};
use ratatui::{prelude::*, style::palette::tailwind, widgets::*};
use tokio::{spawn, task::JoinHandle};
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;

use crate::widgets::{
    events::EventsView,
    help::HelpView,
    linked_chunk::LinkedChunkView,
    read_receipts::ReadReceipts,
    room_list::{ExtraRoomInfo, RoomInfos, RoomList, Rooms},
    status::Status,
};

mod widgets;

const HEADER_BG: Color = tailwind::BLUE.c950;
const NORMAL_ROW_COLOR: Color = tailwind::SLATE.c950;
const ALT_ROW_COLOR: Color = tailwind::SLATE.c900;
const SELECTED_STYLE_FG: Color = tailwind::BLUE.c300;
const TEXT_COLOR: Color = tailwind::SLATE.c200;

type UiRooms = Arc<Mutex<HashMap<OwnedRoomId, room_list_service::Room>>>;
type Timelines = Arc<Mutex<HashMap<OwnedRoomId, Timeline>>>;

#[derive(Debug, Parser)]
struct Cli {
    /// The homeserver the client should connect to.
    server_name: String,

    /// The path where session specific data should be stored.
    #[clap(default_value = "/tmp/")]
    session_path: PathBuf,

    /// Set the proxy that should be used for the connection.
    #[clap(short, long, env = "PROXY")]
    proxy: Option<Url>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlobalMode {
    #[default]
    Default,
    Help,
}

/// Helper function to create a centered rect using up certain percentage of the
/// available rect `r`
fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

#[tokio::main]
async fn main() -> Result<()> {
    let file_writer = tracing_appender::rolling::hourly("/tmp/", "logs-");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(file_writer)
        .init();

    color_eyre::install()?;

    let cli = Cli::parse();
    let client = configure_client(cli).await?;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let terminal = ratatui::init();
    let mut app = App::new(client).await?;

    app.run(terminal).await
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum DetailsMode {
    ReadReceipts,
    #[default]
    TimelineItems,
    Events,
    LinkedChunk,
}

struct Timeline {
    timeline: Arc<SdkTimeline>,
    items: Arc<Mutex<Vector<Arc<TimelineItem>>>>,
    task: JoinHandle<()>,
}

struct App {
    /// Reference to the main SDK client.
    client: Client,

    /// The sync service used for synchronizing events.
    sync_service: Arc<SyncService>,

    /// Room list service rooms known to the app.
    ui_rooms: UiRooms,

    /// Timelines data structures for each room.
    timelines: Timelines,

    /// The room list widget on the left-hand side of the screen.
    room_list: RoomList,

    /// Task listening to room list service changes, and spawning timelines.
    listen_task: JoinHandle<()>,

    /// The status widet at the bottom of the screen.
    status: Status,

    /// What's shown in the details view, aka the right panel.
    details_mode: DetailsMode,

    current_pagination: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// What popup are we showing that is covering the majority of the screen,
    /// mainly used for help and settings screens.
    global_mode: GlobalMode,
}

impl App {
    async fn new(client: Client) -> Result<Self> {
        let sync_service = Arc::new(SyncService::builder(client.clone()).build().await?);

        let rooms = Rooms::default();
        let room_infos = RoomInfos::default();
        let ui_rooms = UiRooms::default();
        let timelines = Timelines::default();

        let room_list_service = sync_service.room_list_service();
        let all_rooms = room_list_service.all_rooms().await?;

        let listen_task = spawn(Self::listen_task(
            rooms.clone(),
            room_infos.clone(),
            ui_rooms.clone(),
            timelines.clone(),
            all_rooms,
        ));

        // This will sync (with encryption) until an error happens or the program is
        // stopped.
        sync_service.start().await;

        let status = Status::new();
        let room_list = RoomList::new(
            rooms,
            ui_rooms.clone(),
            room_infos,
            sync_service.clone(),
            status.handle(),
        );

        Ok(Self {
            sync_service,
            ui_rooms,
            room_list,
            client,
            listen_task,
            status,
            global_mode: GlobalMode::default(),
            details_mode: Default::default(),
            timelines,
            current_pagination: Default::default(),
        })
    }

    async fn listen_task(
        rooms: Rooms,
        room_infos: RoomInfos,
        ui_rooms: UiRooms,
        timelines: Timelines,
        all_rooms: room_list_service::RoomList,
    ) {
        let (stream, entries_controller) = all_rooms.entries_with_dynamic_adapters(50_000);
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
            let previous_ui_rooms = ui_rooms.lock().clone();

            let mut new_ui_rooms = HashMap::new();
            let mut new_timelines = Vec::new();

            // Update all the room info for all rooms.
            for room in all_rooms.iter() {
                let raw_name = room.name();
                let display_name = room.cached_display_name();
                let is_dm = room
                    .is_direct()
                    .await
                    .map_err(|err| {
                        warn!("couldn't figure whether a room is a DM or not: {err}");
                    })
                    .ok();
                room_infos.lock().unwrap().insert(
                    room.room_id().to_owned(),
                    ExtraRoomInfo { raw_name, display_name, is_dm },
                );
            }

            // Initialize all the new rooms.
            for ui_room in
                all_rooms.into_iter().filter(|room| !previous_ui_rooms.contains_key(room.room_id()))
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
                    continue;
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
                    while let Some(diffs) = stream.next().await {
                        let mut items = items.lock();

                        for diff in diffs {
                            diff.apply(&mut items);
                        }
                    }
                });

                new_timelines.push((
                    ui_room.room_id().to_owned(),
                    Timeline { timeline: sdk_timeline, items, task: timeline_task },
                ));

                // Save the room list service room in the cache.
                new_ui_rooms.insert(ui_room.room_id().to_owned(), ui_room);
            }

            ui_rooms.lock().extend(new_ui_rooms);
            timelines.lock().extend(new_timelines);
        }
    }

    async fn toggle_reaction_to_latest_msg(&mut self) {
        let selected = self.room_list.get_selected_room_id();

        if let Some((sdk_timeline, items)) = selected.and_then(|room_id| {
            self.timelines
                .lock()
                .get(&room_id)
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
                        self.status.set_message("reaction sent!".to_owned());
                    }
                    Err(err) => self.status.set_message(format!("error when reacting: {err}")),
                }
            } else {
                self.status.set_message("no item to react to".to_owned());
            }
        } else {
            self.status.set_message("missing timeline for room".to_owned());
        };
    }

    /// Run a small back-pagination (expect a batch of 20 events, continue until
    /// we get 10 timeline items or hit the timeline start).
    fn back_paginate(&mut self) {
        let Some(sdk_timeline) = self.room_list.get_selected_room_id().and_then(|room_id| {
            self.timelines.lock().get(&room_id).map(|timeline| timeline.timeline.clone())
        }) else {
            self.status.set_message("missing timeline for room".to_owned());
            return;
        };

        let mut pagination = self.current_pagination.lock();

        // Cancel the previous back-pagination, if any.
        if let Some(prev) = pagination.take() {
            prev.abort();
        }

        let status_handle = self.status.handle();

        // Start a new one, request batches of 20 events, stop after 10 timeline items
        // have been added.
        *pagination = Some(spawn(async move {
            if let Err(err) = sdk_timeline.paginate_backwards(20).await {
                status_handle.set_message(format!("Error during backpagination: {err}"));
            }
        }));
    }

    fn set_mode(&mut self, mode: DetailsMode) {
        self.details_mode = mode;
        self.status.set_mode(mode);
    }

    fn set_global_mode(&mut self, mode: GlobalMode) {
        self.global_mode = mode;
        self.status.set_global_mode(mode);
    }

    async fn handle_help_key_press(&mut self, key: KeyEvent) -> Result<()> {
        use KeyCode::*;

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, Char('q')) => self.set_global_mode(GlobalMode::Default),
            _ => (),
        }

        Ok(())
    }

    async fn handle_global_key_press(&mut self, key: KeyEvent) -> Result<bool> {
        use KeyCode::*;

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, F(1)) => self.set_global_mode(GlobalMode::Help),

            _ => (),
        }

        if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE {
            match key.code {
                Char('q') | Esc => {
                    if self.global_mode != GlobalMode::Default {
                        self.set_global_mode(GlobalMode::Default);
                    } else {
                        return Ok(true);
                    }
                }

                Char('j') | Down => {
                    self.room_list.next_room();
                }

                Char('k') | Up => {
                    self.room_list.previous_room();
                }

                Char('s') => self.sync_service.start().await,
                Char('S') => self.sync_service.stop().await,

                Char('Q') => {
                    let q = self.client.send_queue();
                    let enabled = q.is_enabled();
                    q.set_enabled(!enabled).await;
                }

                Char('M') => {
                    let selected = self.room_list.get_selected_room_id();

                    if let Some(sdk_timeline) = selected.and_then(|room_id| {
                        self.timelines
                            .lock()
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
                                self.status.set_message("message sent!".to_owned());
                            }
                            Err(err) => {
                                self.status.set_message(format!("error when sending event: {err}"));
                            }
                        }
                    } else {
                        self.status.set_message("missing timeline for room".to_owned());
                    };
                }

                Char('L') => self.toggle_reaction_to_latest_msg().await,

                Char('r') => {
                    if self.room_list.get_selected_room_id().is_some() {
                        self.set_mode(DetailsMode::ReadReceipts);
                    }
                }
                Char('t') => self.set_mode(DetailsMode::TimelineItems),
                Char('e') => self.set_mode(DetailsMode::Events),
                Char('l') => self.set_mode(DetailsMode::LinkedChunk),

                Char('b')
                    if self.details_mode == DetailsMode::TimelineItems
                        || self.details_mode == DetailsMode::LinkedChunk =>
                {
                    self.back_paginate();
                }

                Char('m') if self.details_mode == DetailsMode::ReadReceipts => {
                    self.room_list.mark_as_read().await
                }

                _ => {}
            }
        }

        Ok(false)
    }

    async fn render_loop(&mut self, mut terminal: Terminal<impl Backend>) -> Result<()> {
        loop {
            terminal.draw(|f| f.render_widget(&mut *self, f.area()))?;

            if event::poll(Duration::from_millis(16))? {
                if let Event::Key(key) = event::read()? {
                    match self.global_mode {
                        GlobalMode::Default => {
                            if self.handle_global_key_press(key).await? {
                                break;
                            }
                        }
                        GlobalMode::Help => self.handle_help_key_press(key).await?,
                    }
                }
            }
        }

        Ok(())
    }

    async fn run(&mut self, terminal: Terminal<impl Backend>) -> Result<()> {
        self.render_loop(terminal).await?;

        // At this point the user has exited the loop, so shut down the application.
        ratatui::restore();

        println!("Stopping the sync service...");

        self.sync_service.stop().await;
        self.listen_task.abort();

        for timeline in self.timelines.lock().values() {
            timeline.task.abort();
        }

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
        let [header_area, rest_area, status_area] = vertical.areas(area);

        // Create two chunks with equal horizontal screen space. One for the list and
        // the other for the info block.
        let horizontal =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]);
        let [room_list_area, rhs] = horizontal.areas(rest_area);

        self.render_title(header_area, buf);
        self.room_list.render(room_list_area, buf);
        self.render_right(rhs, buf);
        self.status.render(status_area, buf);

        match self.global_mode {
            GlobalMode::Default => (),
            GlobalMode::Help => {
                let mut help_view = HelpView::new();
                help_view.render(area, buf);
            }
        }
    }
}

impl App {
    /// Render the top square (title of the program).
    fn render_title(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("Multiverse").bold().centered().render(area, buf);
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

        if let Some(room_id) = self.room_list.get_selected_room_id() {
            match self.details_mode {
                DetailsMode::ReadReceipts => {
                    // In read receipts mode, show the read receipts object as computed by the
                    // client.
                    let rooms = self.ui_rooms.lock();
                    let room = rooms.get(&room_id);

                    let mut read_receipts = ReadReceipts::new(room);
                    read_receipts.render(inner_area, buf);
                }

                DetailsMode::TimelineItems => {
                    if !self.render_timeline(&room_id, inner_block.clone(), inner_area, buf) {
                        render_paragraph(buf, "(room's timeline disappeared)".to_owned())
                    }
                }

                DetailsMode::LinkedChunk => {
                    // In linked chunk mode, show a rough representation of the chunks.
                    let rooms = self.ui_rooms.lock();
                    let room = rooms.get(&room_id);

                    let mut linked_chunk_view = LinkedChunkView::new(room);
                    linked_chunk_view.render(inner_area, buf);
                }

                DetailsMode::Events => {
                    let rooms = self.ui_rooms.lock();
                    let room = rooms.get(&room_id);

                    let mut events_view = EventsView::new(room);
                    events_view.render(inner_area, buf);
                }
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
        let Some(items) = self.timelines.lock().get(room_id).map(|timeline| timeline.items.clone())
        else {
            return false;
        };

        let items = items.lock();
        let mut content = Vec::new();

        for item in items.iter() {
            match item.kind() {
                TimelineItemKind::Event(ev) => {
                    let sender = ev.sender();

                    match ev.content() {
                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Message(message),
                            ..
                        }) => {
                            if let MessageType::Text(text) = message.msgtype() {
                                content.push(format!("{}: {}", sender, text.body))
                            }
                        }

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Redacted,
                            ..
                        }) => content.push(format!("{}: -- redacted --", sender)),

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::UnableToDecrypt(_),
                            ..
                        }) => content.push(format!("{}: (UTD)", sender)),

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Sticker(_),
                            ..
                        })
                        | TimelineItemContent::MembershipChange(_)
                        | TimelineItemContent::ProfileChange(_)
                        | TimelineItemContent::OtherState(_)
                        | TimelineItemContent::FailedToParseMessageLike { .. }
                        | TimelineItemContent::FailedToParseState { .. }
                        | TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Poll(_),
                            ..
                        })
                        | TimelineItemContent::CallInvite
                        | TimelineItemContent::CallNotify => {
                            continue;
                        }
                    }
                }

                TimelineItemKind::Virtual(virt) => match virt {
                    VirtualTimelineItem::DateDivider(unix_ts) => {
                        content.push(format!("Date: {unix_ts:?}"));
                    }
                    VirtualTimelineItem::ReadMarker => {
                        content.push("Read marker".to_owned());
                    }
                    VirtualTimelineItem::TimelineStart => {
                        content.push("🥳 Timeline start! 🥳".to_owned());
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
}

/// Configure the client so it's ready for sync'ing.
///
/// Will log in or reuse a previous session.
async fn configure_client(cli: Cli) -> Result<Client> {
    let Cli { server_name, session_path, proxy } = cli;

    let mut client_builder = Client::builder()
        .store_config(
            StoreConfig::new("multiverse".to_owned())
                .crypto_store(SqliteCryptoStore::open(session_path.join("crypto"), None).await?)
                .state_store(SqliteStateStore::open(session_path.join("state"), None).await?)
                .event_cache_store(
                    SqliteEventCacheStore::open(session_path.join("cache"), None).await?,
                ),
        )
        .server_name_or_homeserver_url(&server_name)
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            auto_enable_backups: true,
        });

    if let Some(proxy_url) = proxy {
        client_builder = client_builder.proxy(proxy_url).disable_ssl_verification();
    }

    let client = client_builder.build().await?;

    // Try reading a session, otherwise create a new one.
    log_in_or_restore_session(&client, &session_path).await?;

    Ok(client)
}

async fn log_in_or_restore_session(client: &Client, session_path: &Path) -> Result<()> {
    let session_path = session_path.join("session.json");

    if let Ok(serialized) = std::fs::read_to_string(&session_path) {
        let session: MatrixSession = serde_json::from_str(&serialized)?;
        client.restore_session(session).await?;

        println!("restored session");
    } else {
        login_with_password(client).await?;
        println!("new login");

        // Immediately save the session to disk.
        if let Some(session) = client.session() {
            let AuthSession::Matrix(session) = session else {
                panic!("unexpected OAuth 2.0 session")
            };
            let serialized = serde_json::to_string(&session)?;
            std::fs::write(session_path, serialized)?;

            println!("saved session");
        }
    }

    Ok(())
}

/// Asks the user of a username and password, and try to login using the matrix
/// auth with those.
async fn login_with_password(client: &Client) -> Result<()> {
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
