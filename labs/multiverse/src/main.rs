#![allow(clippy::large_enum_variant)]

use std::{
    collections::{HashMap, HashSet},
    io::{self, Write, stdout},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use color_eyre::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
};
use futures_util::{StreamExt as _, pin_mut};
use imbl::Vector;
use layout::Flex;
use matrix_sdk::{
    AuthSession, Client, Room, SqliteCryptoStore, SqliteEventCacheStore, SqliteStateStore,
    ThreadingSupport,
    authentication::matrix::MatrixSession,
    config::StoreConfig,
    deserialized_responses::TimelineEvent,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    reqwest::Url,
    ruma::{
        OwnedEventId, OwnedRoomId, api::client::room::create_room::v3::Request as CreateRoomRequest,
    },
    search_index::{SearchIndexGuard, SearchIndexStoreKind},
};
use matrix_sdk_base::{RoomStateFilter, event_cache::store::EventCacheStoreLockGuard};
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_ui::{
    Timeline as SdkTimeline,
    room_list_service::{self, State, filters::new_filter_non_left},
    sync_service::SyncService,
    timeline::{RoomExt as _, TimelineFocus, TimelineItem},
};
use ratatui::{prelude::*, style::palette::tailwind, widgets::*};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::{
    spawn,
    sync::mpsc::{Receiver, Sender, channel, error::TryRecvError},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, error, warn};
use tracing_subscriber::EnvFilter;
use widgets::{
    recovery::create_centered_throbber_area, room_view::RoomView, settings::SettingsView,
};

use crate::widgets::{
    create_room::CreateRoomView,
    help::HelpView,
    room_list::{ExtraRoomInfo, RoomInfos, RoomList, Rooms},
    search::{
        indexing::{IndexingMessage, IndexingView},
        searching::SearchingView,
    },
    status::Status,
};

mod widgets;

const HEADER_BG: Color = tailwind::BLUE.c950;
const NORMAL_ROW_COLOR: Color = tailwind::SLATE.c950;
const ALT_ROW_COLOR: Color = tailwind::SLATE.c900;
const SELECTED_STYLE_FG: Color = tailwind::BLUE.c300;
const TEXT_COLOR: Color = tailwind::SLATE.c200;

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

    /// Whether to *not* reload the `pos`ition sliding sync token from disk at
    /// start or not, for the room list sliding sync.
    ///
    /// Set to false by default (i.e. reload the position from disk).
    #[clap(short, long, default_value_t = false)]
    dont_share_pos: bool,
}

#[derive(Default)]
pub enum GlobalMode {
    /// The default mode, no popout screen is opened.
    #[default]
    Default,
    /// Mode where we have opened the help screen.
    Help,
    /// Mode where we have opened the settings screen.
    Settings { view: SettingsView },
    /// Mode where we are shutting our tasks down and exiting multiverse.
    Exiting { shutdown_task: JoinHandle<()> },
    /// Mode where we have opened the create room screen
    CreateRoom { view: CreateRoomView },
    /// Mode where we have opened the search screen
    Searching { view: SearchingView },
    /// Mode where we have opened the indexing screen
    Indexing { view: IndexingView },
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
    let cli = Cli::parse();
    let file_writer = tracing_appender::rolling::hourly(&cli.session_path, "logs-");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(file_writer)
        .init();

    color_eyre::install()?;

    let share_pos = !cli.dont_share_pos;
    let client = configure_client(cli).await?;

    let event_cache = client.event_cache();
    event_cache.subscribe()?;

    let terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let mut app = App::new(client, share_pos).await?;

    app.run(terminal).await
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

    /// State for a global throbber.
    throbber_state: ThrobberState,
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

    /// Task that is indexing events for search.
    indexing_task: JoinHandle<()>,

    /// Receiver that notifies when indexing is complete.
    indexing_receiver: Receiver<(bool, IndexingMessage)>,

    /// The status widget at the bottom of the screen.
    status: Status,

    state: AppState,

    last_tick: Instant,
}

impl App {
    const TICK_RATE: Duration = Duration::from_millis(250);

    async fn new(client: Client, share_pos: bool) -> Result<Self> {
        let sync_service =
            Arc::new(SyncService::builder(client.clone()).with_share_pos(share_pos).build().await?);

        let rooms = Rooms::default();
        let room_infos = RoomInfos::default();
        let timelines = Timelines::default();

        let room_list_service = sync_service.room_list_service();
        let all_rooms = room_list_service.all_rooms().await?;

        let listen_task = spawn(Self::listen_task(
            rooms.clone(),
            room_infos.clone(),
            timelines.clone(),
            all_rooms,
        ));

        // This will sync (with encryption) until an error happens or the program is
        // stopped.
        sync_service.start().await;

        let status = Status::new();
        let room_list =
            RoomList::new(client.clone(), rooms, room_infos, sync_service.clone(), status.handle());

        let room_view = RoomView::new(client.clone(), timelines.clone(), status.handle());

        let (indexing_sender, indexing_receiver) = channel::<(bool, IndexingMessage)>(1024);
        let indexing_task =
            spawn(App::indexing_task(client.clone(), indexing_sender, sync_service.clone()));

        let indexing_view = IndexingView::new();

        Ok(Self {
            sync_service,
            timelines,
            room_list,
            room_view,
            client,
            listen_task,
            indexing_task,
            indexing_receiver,
            status,
            state: AppState {
                global_mode: GlobalMode::Indexing { view: indexing_view },
                ..Default::default()
            },
            last_tick: Instant::now(),
        })
    }

    async fn listen_task(
        rooms: Rooms,
        room_infos: RoomInfos,
        timelines: Timelines,
        all_rooms: room_list_service::RoomList,
    ) {
        let (stream, entries_controller) = all_rooms.entries_with_dynamic_adapters(50_000);
        entries_controller.set_filter(Box::new(new_filter_non_left()));

        pin_mut!(stream);

        let mut previous_rooms = HashSet::new();

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

            let mut new_rooms = HashMap::new();
            let mut new_timelines = Vec::new();

            // Update all the room info for all rooms.
            for room in all_rooms.iter() {
                let raw_name = room.name();
                let display_name =
                    room.cached_display_name().map(|display_name| display_name.to_string());
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
            for room in
                all_rooms.into_iter().filter(|room| !previous_rooms.contains(room.room_id()))
            {
                // Initialize the timeline.
                let Ok(timeline) = room
                    .timeline_builder()
                    .with_focus(TimelineFocus::Live { hide_threaded_events: true })
                    .build()
                    .await
                else {
                    error!("error when creating default timeline");
                    continue;
                };

                // Save the timeline in the cache.
                let (items, stream) = timeline.subscribe().await;
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
                    room.room_id().to_owned(),
                    Timeline { timeline: Arc::new(timeline), items, task: timeline_task },
                ));

                // Save the room list service room in the cache.
                new_rooms.insert(room.room_id().to_owned(), room);
            }

            previous_rooms.extend(new_rooms.into_keys());

            timelines.lock().extend(new_timelines);
        }
    }

    async fn wait_for_room_sync(
        update_sender: &Sender<(bool, IndexingMessage)>,
        sync_service: Arc<SyncService>,
    ) {
        let mut sync_subscriber = sync_service.room_list_service().state();

        // Spin until there are rooms to index
        while let Some(state) = sync_subscriber.next().await {
            match state {
                State::Running => return,
                State::Terminated { from: _prev } => {
                    while let Err(e) =
                        update_sender.send((true, IndexingMessage::Progress(0))).await
                    {
                        debug!("Failed to send final message, trying again: {e:?}");
                    }
                    return;
                }
                _ => {
                    debug!("Sync service not running. Waiting to start indexing. {state:?}");
                }
            }
        }
    }

    async fn index_event_cache(
        client: &Client,
        update_sender: &Sender<(bool, IndexingMessage)>,
        store: &EventCacheStoreLockGuard,
        search_index_guard: &mut SearchIndexGuard<'_>,
        mut count: usize,
    ) -> Result<usize, ()> {
        for room in client.rooms_filtered(RoomStateFilter::JOINED.union(RoomStateFilter::LEFT)) {
            let room_id = room.room_id();

            let maybe_room_cache = room.event_cache().await;
            let Ok((room_cache, _drop_handles)) = maybe_room_cache else {
                warn!("Failed to get RoomEventCache: {maybe_room_cache:?}");
                continue;
            };

            let redaction_rules = room.clone_info().room_version_rules_or_default().redaction;

            let maybe_timeline_events = store.get_room_events(room_id, None, None).await;
            let Ok(timeline_events) = maybe_timeline_events else {
                warn!("Failed to get room's events: {maybe_timeline_events:?}");
                continue;
            };

            let no_of_events = timeline_events.len();

            if let Err(err) = search_index_guard
                .bulk_handle_timeline_event(
                    timeline_events.clone().into_iter(),
                    &room_cache,
                    room_id,
                    &redaction_rules,
                )
                .await
            {
                error!("Failed to handle event for indexing: {err}");
                let mut error = Some(err);
                while let Some(err) = error.take() {
                    if let Err(e) =
                        update_sender.send((true, IndexingMessage::Error(err.to_string()))).await
                    {
                        debug!("Failed to send final error message, trying again: {e:?}");
                    }
                }
                return Err(());
            }

            count += no_of_events;
            let _ = update_sender.send((false, IndexingMessage::Progress(count))).await;
        }
        Ok(count)
    }

    async fn index_from_server(
        client: &Client,
        update_sender: &Sender<(bool, IndexingMessage)>,
        search_index_guard: &mut SearchIndexGuard<'_>,
        mut count: usize,
    ) -> Result<usize, ()> {
        let batch_size = 25;

        let mut rooms = client.rooms_filtered(RoomStateFilter::JOINED);
        let mut idx = 0;

        while !rooms.is_empty() {
            let room = &rooms[idx];

            let room_id = room.room_id();

            let maybe_room_cache = room.event_cache().await;
            let Ok((room_cache, _drop_handles)) = maybe_room_cache else {
                warn!("Failed to get RoomEventCache: {maybe_room_cache:?}");
                idx = (idx + 1) % rooms.len();
                continue;
            };

            let redaction_rules = room.clone_info().room_version_rules_or_default().redaction;

            let Ok(pagination) = room_cache.pagination().run_backwards_until(batch_size).await
            else {
                error!("Failed to backpaginate {room_id}");
                idx = (idx + 1) % rooms.len();
                continue;
            };

            let no_of_events = pagination.events.len();

            if let Err(err) = search_index_guard
                .bulk_handle_timeline_event(
                    pagination.events.clone().into_iter(),
                    &room_cache,
                    room_id,
                    &redaction_rules,
                )
                .await
            {
                warn!("Failed to handle event for indexing: {err}");
                let mut error = Some(err);
                while let Some(err) = error.take() {
                    if let Err(e) =
                        update_sender.send((true, IndexingMessage::Error(err.to_string()))).await
                    {
                        debug!("Failed to send final error message, trying again: {e:?}");
                    }
                }
                return Err(());
            }

            count += no_of_events;
            let _ = update_sender.send((false, IndexingMessage::Progress(count))).await;

            if pagination.reached_start {
                rooms.remove(idx);
                let len = rooms.len();
                if len > 0 {
                    idx %= len;
                }
            } else {
                idx = (idx + 1) % rooms.len();
            }
        }
        Ok(count)
    }

    /// The sender sends (progress, done?, error?).
    async fn indexing_task(
        client: Client,
        update_sender: Sender<(bool, IndexingMessage)>,
        sync_service: Arc<SyncService>,
    ) {
        if timeout(Duration::from_secs(30), App::wait_for_room_sync(&update_sender, sync_service))
            .await
            .is_err()
        {
            debug!("Waiting for sync to run timed out. Quitting indexing task.");
            return;
        }

        let Ok(store) = client.event_cache_store().lock().await else {
            error!("Failed to get EventCacheStore");
            return;
        };

        let mut search_index_guard = client.search_index().lock().await;
        let count = 0;

        debug!("Start indexing from the event cache.");

        // First index everything in the cache
        let Ok(count) = App::index_event_cache(
            &client,
            &update_sender,
            store.as_clean().expect("Only one process should access the event cache store"),
            &mut search_index_guard,
            count,
        )
        .await
        else {
            debug!("Quitting index task.");
            return;
        };

        // Now index from the server
        debug!("Start indexing from the server.");

        let Ok(count) =
            App::index_from_server(&client, &update_sender, &mut search_index_guard, count).await
        else {
            debug!("Quitting index task.");
            return;
        };

        while let Err(err) = update_sender.send((true, IndexingMessage::Progress(count))).await {
            debug!("couldn't send final update {err}, trying again.");
        }
    }

    fn set_global_mode(&mut self, mode: GlobalMode) {
        self.state.global_mode = mode;
    }

    async fn handle_global_event(&mut self, event: Event) -> Result<bool> {
        use KeyCode::*;

        match event {
            Event::Key(KeyEvent { code: F(1), modifiers: KeyModifiers::NONE, .. }) => {
                self.set_global_mode(GlobalMode::Help)
            }

            Event::Key(KeyEvent { code: F(10), modifiers: KeyModifiers::NONE, .. }) => self
                .set_global_mode(GlobalMode::Settings {
                    view: SettingsView::new(self.client.clone(), self.sync_service.clone()),
                }),

            Event::Key(KeyEvent {
                code: Char('j') | Down,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.room_list.next_room().await;
                let room_id = self.room_list.get_selected_room_id();
                self.room_view.set_selected_room(room_id);
            }

            Event::Key(KeyEvent {
                code: Char('k') | Up, modifiers: KeyModifiers::CONTROL, ..
            }) => {
                self.room_list.previous_room().await;
                let room_id = self.room_list.get_selected_room_id();
                self.room_view.set_selected_room(room_id);
            }

            Event::Key(KeyEvent { code: Char('m'), modifiers: KeyModifiers::ALT, .. }) => {
                self.room_view.mark_as_read().await
            }

            Event::Key(KeyEvent { code: Char('q'), modifiers: KeyModifiers::CONTROL, .. }) => {
                if !matches!(self.state.global_mode, GlobalMode::Default) {
                    self.set_global_mode(GlobalMode::Default);
                } else {
                    return Ok(true);
                }
            }

            Event::Key(KeyEvent { modifiers: KeyModifiers::CONTROL, code: Char('r'), .. }) => {
                self.set_global_mode(GlobalMode::CreateRoom { view: CreateRoomView::new() })
            }

            Event::Key(KeyEvent { modifiers: KeyModifiers::CONTROL, code: Char('s'), .. }) => {
                self.set_global_mode(GlobalMode::Searching { view: SearchingView::new() })
            }

            _ => self.room_view.handle_event(event).await,
        }

        Ok(false)
    }

    fn on_tick(&mut self) {
        self.state.throbber_state.calc_next();

        match &mut self.state.global_mode {
            GlobalMode::Help
            | GlobalMode::Default
            | GlobalMode::CreateRoom { .. }
            | GlobalMode::Searching { .. }
            | GlobalMode::Exiting { .. } => {}
            GlobalMode::Settings { view } => {
                view.on_tick();
            }
            GlobalMode::Indexing { view } => {
                view.on_tick();
            }
        }
    }

    async fn render_loop(&mut self, mut terminal: Terminal<impl Backend>) -> Result<()> {
        use KeyCode::*;

        let mut check_channel = true;

        loop {
            if check_channel {
                match self.indexing_receiver.try_recv() {
                    Ok((done, message)) => {
                        if !matches!(message, IndexingMessage::Error(_)) && done {
                            self.set_global_mode(GlobalMode::Default);
                        } else if let GlobalMode::Indexing { view } = &mut self.state.global_mode {
                            view.set_message(message);
                        }
                    }
                    Err(TryRecvError::Disconnected) => check_channel = false,
                    Err(TryRecvError::Empty) => {}
                }
            }

            terminal.draw(|f| f.render_widget(&mut *self, f.area()))?;

            if event::poll(Duration::from_millis(100))? {
                let event = event::read()?;

                match &mut self.state.global_mode {
                    GlobalMode::Default => {
                        if self.handle_global_event(event).await? {
                            let sync_service = self.sync_service.clone();
                            let timelines = self.timelines.clone();
                            let listen_task = self.listen_task.abort_handle();
                            let indexing_task = self.indexing_task.abort_handle();

                            let shutdown_task = spawn(async move {
                                sync_service.stop().await;

                                listen_task.abort();
                                indexing_task.abort();

                                for timeline in timelines.lock().values() {
                                    timeline.task.abort();
                                }
                            });

                            self.set_global_mode(GlobalMode::Exiting { shutdown_task });
                        }
                    }
                    GlobalMode::Help => {
                        if let Event::Key(key) = event
                            && let KeyModifiers::NONE = key.modifiers
                            && let Char('q') | Esc = key.code
                        {
                            self.set_global_mode(GlobalMode::Default)
                        }
                    }
                    GlobalMode::Settings { view } => {
                        if let Event::Key(key) = event
                            && view.handle_key_press(key).await
                        {
                            self.set_global_mode(GlobalMode::Default);
                        }
                    }
                    GlobalMode::CreateRoom { view } => {
                        if let Event::Key(key) = event
                            && let KeyModifiers::NONE = key.modifiers
                        {
                            match key.code {
                                Enter => {
                                    if let Some(room_name) = view.get_text() {
                                        let mut request = CreateRoomRequest::new();
                                        request.name = Some(room_name);
                                        if let Err(err) = self
                                            .sync_service
                                            .room_list_service()
                                            .client()
                                            .create_room(request)
                                            .await
                                        {
                                            error!("error while creating room: {err:?}");
                                        }
                                    }
                                    self.set_global_mode(GlobalMode::Default);
                                }
                                Esc => self.set_global_mode(GlobalMode::Default),
                                _ => view.handle_key_press(key),
                            }
                        }
                    }
                    GlobalMode::Searching { view } => {
                        if let Event::Key(key) = event {
                            match key.code {
                                Enter => {
                                    if let Some(query) = view.get_text() {
                                        if let Some(room) = self.room_view.room() {
                                            if let Ok(results) =
                                                room.search(&query, 100, None).await.inspect_err(|err| {
                                                    error!("error occurred while searching index: {err:?}");
                                                })
                                            {
                                                let results = get_events_from_event_ids(
                                                    &self.client,
                                                    &room,
                                                    results,
                                                )
                                                .await;

                                                view.results(results);
                                            }
                                        } else {
                                            warn!("No room in view.")
                                        }
                                    }
                                }
                                Esc => self.set_global_mode(GlobalMode::Default),
                                Up => view.list_state.previous(),
                                Down => view.list_state.next(),
                                _ => view.handle_key_press(key),
                            }
                        }
                    }
                    GlobalMode::Indexing { .. } => {
                        if let Event::Key(key) = event
                            && let KeyModifiers::NONE = key.modifiers
                            && let Esc = key.code
                        {
                            self.indexing_task.abort();
                            self.set_global_mode(GlobalMode::Default);
                        }
                    }
                    GlobalMode::Exiting { .. } => {}
                }
            }

            match &self.state.global_mode {
                GlobalMode::Default
                | GlobalMode::Help
                | GlobalMode::CreateRoom { .. }
                | GlobalMode::Searching { .. }
                | GlobalMode::Indexing { .. }
                | GlobalMode::Settings { .. } => {}
                GlobalMode::Exiting { shutdown_task } => {
                    if shutdown_task.is_finished() {
                        break;
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
        execute!(stdout(), DisableMouseCapture)?;

        Ok(())
    }
}

impl Widget for &mut App {
    /// Render the whole app.
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Create a space for header, room list and timeline and the footer.
        let vertical =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)]);
        let [header_area, rest_area, status_area] = vertical.areas(area);

        // Create two chunks with equal horizontal screen space. One for the list and
        // the other for the info block.
        let horizontal =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Percentage(75)]);
        let [room_list_area, room_view_area] = horizontal.areas(rest_area);

        self.render_title(header_area, buf);
        self.room_list.render(room_list_area, buf);
        self.room_view.render(room_view_area, buf);
        self.status.render(status_area, buf, &mut self.state);

        match &mut self.state.global_mode {
            GlobalMode::Default => {}
            GlobalMode::Exiting { .. } => {
                Clear.render(rest_area, buf);
                let centered = create_centered_throbber_area(area);
                let throbber = Throbber::default()
                    .label("Exiting")
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                StatefulWidget::render(throbber, centered, buf, &mut self.state.throbber_state);
            }
            GlobalMode::Settings { view } => {
                view.render(area, buf);
            }
            GlobalMode::Help => {
                let mut help_view = HelpView::new();
                help_view.render(area, buf);
            }
            GlobalMode::CreateRoom { view } => {
                view.render(area, buf);
            }
            GlobalMode::Searching { view } => {
                view.render(room_view_area, buf);
            }
            GlobalMode::Indexing { view } => {
                view.render(area, buf);
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
    let Cli { server_name, session_path, proxy, dont_share_pos: _ } = cli;

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
        })
        .with_enable_share_history_on_invite(true)
        .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        .search_index_store(SearchIndexStoreKind::UnencryptedDirectory(
            session_path.join("indexData"),
        ));

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
    } else {
        login_with_password(client).await?;

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

async fn get_events_from_event_ids(
    client: &Client,
    room: &Room,
    event_ids: Vec<OwnedEventId>,
) -> Vec<TimelineEvent> {
    if let Ok(cache_lock) = client.event_cache_store().lock().await {
        let cache_lock =
            cache_lock.as_clean().expect("Only one process must access the event cache store");

        futures_util::future::join_all(event_ids.iter().map(|event_id| async {
            let event_id = event_id.clone();
            match cache_lock.find_event(room.room_id(), &event_id).await {
                Ok(ev) => ev,
                Err(_) => room
                    .event(&event_id, None)
                    .await
                    .inspect_err(|err| {
                        debug!("Failed to find event {event_id} in event cache and server: {err}");
                    })
                    .ok(),
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<TimelineEvent>>()
    } else {
        debug!("Couldnt get event cache store lock.");
        Vec::new()
    }
}
