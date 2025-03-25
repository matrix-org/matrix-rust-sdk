use std::{
    collections::HashMap,
    io::{self, stdout, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
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
    ruma::OwnedRoomId,
    AuthSession, Client, SqliteCryptoStore, SqliteEventCacheStore, SqliteStateStore,
};
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_ui::{
    room_list_service::{self, filters::new_filter_non_left},
    sync_service::SyncService,
    timeline::TimelineItem,
    Timeline as SdkTimeline,
};
use ratatui::{prelude::*, style::palette::tailwind, widgets::*};
use tokio::{spawn, task::JoinHandle};
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;
use widgets::{
    recovery::{RecoveryView, RecoveryViewState},
    room_view::RoomView,
};

use crate::widgets::{
    help::HelpView,
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

#[derive(Default)]
pub enum GlobalMode {
    /// The default mode, no popout screen is opened.
    #[default]
    Default,
    /// Mode where we have opened the help screen.
    Help,
    /// Mode where we have opened the recovery screen.
    Recovery { state: RecoveryViewState },
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
    event_cache.subscribe()?;
    event_cache.enable_storage()?;

    let terminal = ratatui::init();
    let mut app = App::new(client).await?;

    app.run(terminal).await
}

#[derive(Default, PartialEq)]
pub enum RoomViewDetails {
    /// Show just the timeline.
    #[default]
    None,
    /// Show details about read receipts of the room.
    ReadReceipts,
    /// Show the raw event sources of the timeline.
    Events,
    /// Show the linked chunks that are used to display the timeline.
    LinkedChunk,
}

pub struct Timeline {
    timeline: Arc<SdkTimeline>,
    items: Arc<Mutex<Vector<Arc<TimelineItem>>>>,
    task: JoinHandle<()>,
}

#[derive(Default)]
pub struct AppState {
    /// What popup are we showing that is covering the majority of the screen,
    /// mainly used for help and settings screens.
    global_mode: GlobalMode,

    /// Is any details popup shown above the room view or are we looking at the
    /// timeline.
    details_mode: RoomViewDetails,
}

struct App {
    /// Reference to the main SDK client.
    client: Client,

    /// The sync service used for synchronizing events.
    sync_service: Arc<SyncService>,

    /// Timelines data structures for each room.
    timelines: Timelines,

    /// The room list widget on the left-hand side of the screen.
    room_list: RoomList,

    /// A view displaying the contents of the selected room, the widget on the
    /// right-hand side of the screen.
    room_view: RoomView,

    /// Task listening to room list service changes, and spawning timelines.
    listen_task: JoinHandle<()>,

    /// The status widet at the bottom of the screen.
    status: Status,

    state: AppState,

    last_tick: Instant,
}

impl App {
    const TICK_RATE: Duration = Duration::from_millis(250);

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

        let room_view = RoomView::new(ui_rooms, timelines.clone(), status.handle());

        Ok(Self {
            sync_service,
            timelines,
            room_list,
            room_view,
            client,
            listen_task,
            status,
            state: AppState::default(),
            last_tick: Instant::now(),
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
                let mut rooms = rooms.lock();

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
                room_infos.lock().insert(
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

    fn set_mode(&mut self, mode: RoomViewDetails) {
        self.state.details_mode = mode;
    }

    fn set_global_mode(&mut self, mode: GlobalMode) {
        self.state.global_mode = mode;
    }

    async fn handle_global_key_press(&mut self, key: KeyEvent) -> Result<bool> {
        use KeyCode::*;

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, F(1)) => self.set_global_mode(GlobalMode::Help),
            (KeyModifiers::NONE, F(10)) => self.set_global_mode(GlobalMode::Recovery {
                state: RecoveryViewState::new(self.client.clone()),
            }),

            _ => (),
        }

        if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE {
            match key.code {
                Char('q') | Esc => {
                    if !matches!(self.state.global_mode, GlobalMode::Default) {
                        self.set_global_mode(GlobalMode::Default);
                    } else {
                        return Ok(true);
                    }
                }

                Char('j') | Down => {
                    self.room_list.next_room();
                    let room_id = self.room_list.get_selected_room_id();
                    self.room_view.set_selected_room(room_id);
                }

                Char('k') | Up => {
                    self.room_list.previous_room();
                    let room_id = self.room_list.get_selected_room_id();
                    self.room_view.set_selected_room(room_id);
                }

                Char('s') => self.sync_service.start().await,
                Char('S') => self.sync_service.stop().await,

                Char('Q') => {
                    let q = self.client.send_queue();
                    let enabled = q.is_enabled();
                    q.set_enabled(!enabled).await;
                }

                Char('r') => {
                    if self.room_list.get_selected_room_id().is_some() {
                        self.set_mode(RoomViewDetails::ReadReceipts);
                    }
                }
                Char('t') => self.set_mode(RoomViewDetails::None),
                Char('e') => self.set_mode(RoomViewDetails::Events),
                Char('l') => self.set_mode(RoomViewDetails::LinkedChunk),

                Char('b')
                    if self.state.details_mode == RoomViewDetails::None
                        || self.state.details_mode == RoomViewDetails::LinkedChunk =>
                {
                    self.room_view.back_paginate();
                }

                Char('m') if self.state.details_mode == RoomViewDetails::ReadReceipts => {
                    self.room_view.mark_as_read().await
                }

                _ => {}
            }
        }

        Ok(false)
    }

    fn on_tick(&mut self) {
        match &mut self.state.global_mode {
            GlobalMode::Help | GlobalMode::Default => {}

            GlobalMode::Recovery { state } => {
                state.on_tick();
            }
        }
    }

    async fn render_loop(&mut self, mut terminal: Terminal<impl Backend>) -> Result<()> {
        use KeyCode::*;

        loop {
            terminal.draw(|f| f.render_widget(&mut *self, f.area()))?;

            if event::poll(Duration::from_millis(16))? {
                if let Event::Key(key) = event::read()? {
                    match &mut self.state.global_mode {
                        GlobalMode::Default => {
                            if self.handle_global_key_press(key).await? {
                                break;
                            }
                        }
                        GlobalMode::Help => match (key.modifiers, key.code) {
                            (KeyModifiers::NONE, Char('q')) => {
                                self.set_global_mode(GlobalMode::Default)
                            }
                            _ => (),
                        },
                        GlobalMode::Recovery { state } => {
                            if state.handle_key_press(key) {
                                self.set_global_mode(GlobalMode::Default);
                            }
                        }
                    }
                }
            }

            if self.last_tick.elapsed() >= Self::TICK_RATE {
                self.on_tick();
                self.last_tick = Instant::now();
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
        let [room_list_area, room_view_area] = horizontal.areas(rest_area);

        self.render_title(header_area, buf);
        self.room_list.render(room_list_area, buf);
        self.room_view.render(room_view_area, buf, &mut self.state.details_mode);
        self.status.render(status_area, buf, &mut self.state);

        match &mut self.state.global_mode {
            GlobalMode::Default => {}
            GlobalMode::Recovery { state } => {
                let mut recovery_view = RecoveryView::new();
                recovery_view.render(area, buf, state);
            }
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
