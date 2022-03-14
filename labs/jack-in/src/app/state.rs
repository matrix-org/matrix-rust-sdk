use std::time::{Instant, Duration};
use matrix_sdk::{Client, SlidingSyncView, SlidingSyncRoom};
use tui::widgets::TableState;
use std::collections::btree_map::BTreeMap;
use futures_signals::signal::Mutable;
use log::warn;

#[derive(Clone)]
pub struct Syncv2State {
    started: Instant,
    first_render: Option<Duration>,
    rooms_count: Option<u32>,
}

impl Syncv2State {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            first_render: None,
            rooms_count: None,
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn time_to_first_render(&self) -> Option<Duration> {
        self.first_render.clone()
    }

    pub fn rooms_count(&self) -> Option<u32> {
        self.rooms_count.clone()
    }

    pub fn set_first_render_now(&mut self) {
        self.first_render = Some(self.started.elapsed())
    }
    pub fn set_rooms_count(&mut self, counter: u32) {
        self.rooms_count = Some(counter)
    }
}

#[derive(Clone, Default)]
pub struct CurrentRoomSummary {
    pub name: String,
    pub state_events_counts: Vec<(String, usize)>,
    //pub state_events: BTreeMap<String, Vec<SlidingSyncRoom>>,
}

#[derive(Clone)]
pub struct SlidingSyncState {
    started: Instant,
    view: SlidingSyncView,
    current_room_summary: Mutable<Option<CurrentRoomSummary>>,
    /// the current list selector for the room
    pub rooms_state: TableState,
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    current_rooms_count: Option<u32>,
    total_rooms_count: Option<u32>,
    selected_room: Option<Box<matrix_sdk::ruma::RoomId>>
}

impl SlidingSyncState {
    pub fn new(view: SlidingSyncView) -> Self {
        Self {
            started: Instant::now(),
            view,
            current_room_summary: Mutable::new(None),
            rooms_state: TableState::default(),
            first_render: None,
            full_sync: None,
            selected_room: None,
            current_rooms_count: None,
            total_rooms_count: None,
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn time_to_first_render(&self) -> Option<Duration> {
        self.first_render.clone()
    }

    pub fn time_to_full_sync(&self) -> Option<Duration> {
        self.full_sync.clone()
    }

    pub fn loaded_rooms_count(&self) -> usize {
        self.view.rooms.lock_ref().len()
    }

    pub fn total_rooms_count(&self) -> Option<u64> {
        self.view.rooms_count.get()
    }
    pub fn selected_room(&self) -> Option<Box<matrix_sdk::ruma::RoomId>> {
        self.selected_room.clone()
    }

    pub fn set_first_render_now(&mut self) {
        self.first_render = Some(self.started.elapsed())
    }

    pub fn view(&self) -> &SlidingSyncView {
        &self.view
    }

    pub fn current_room_summary(&self) -> Option<CurrentRoomSummary> {
        if self.current_room_summary.lock_ref().is_none() {

            let room_id = if let Some(id) = &self.selected_room {
                id
            } else {
                return None;
            };

            let room_data = {
                let l = self.view().rooms.lock_ref();
                if let Some(room) = l.get(room_id) {
                    room.clone()
                } else {
                    return None
                }
            };

            let name = room_data.name.clone().unwrap_or_else(|| "unkown".to_owned());

            let state_events = room_data.required_state.iter().filter_map(|r| r.deserialize().ok()).fold(BTreeMap::<String, Vec<_>>::new(), |mut b, r| {
                let event_name = r.event_type().to_owned();
                b.entry(event_name).and_modify(|l| l.push(r.clone())).or_insert_with(|| vec![r.clone()]);
                b
            });

            let mut state_events_counts: Vec<(String, usize)> = state_events.iter().map(|(k, l)| (k.clone(), l.len())).collect();
            state_events_counts.sort_by_key(|(_, count)| *count);

            self.current_room_summary.set(Some(CurrentRoomSummary {
                name, state_events_counts
            }));
        }
        return self.current_room_summary.get_cloned()
    }

    pub fn set_full_sync_now(&mut self) {
        self.full_sync = Some(self.started.elapsed())
    }

    pub fn next_room(&mut self) {
        let i = match self.rooms_state.selected() {
            Some(i) => {
                if i >= self.loaded_rooms_count() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.rooms_state.select(Some(i));
    }

    pub fn previous_room(&mut self) {
        let i = match self.rooms_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.loaded_rooms_count() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.rooms_state.select(Some(i));
    }

    pub fn unselect_room(&mut self) {
        self.rooms_state.select(None);
    }

    pub fn select_room(&mut self) {
        let next_id = if let Some(idx) = self.rooms_state.selected() {
            if let Some(Some(r)) = self.view.rooms_list.lock_ref().get(idx) {
                Some(r.clone())
            } else {
                None
            }
        } else {
            None
        };
        

        self.selected_room = next_id;
        self.current_room_summary.set(None);
    }
}
#[derive(Clone)]
pub enum AppState {
    Init,
    Initialized {
        title: Option<String>,
        v2: Option<Syncv2State>,
        sliding: Option<SlidingSyncState>,
        show_logs: bool
    },
}

impl AppState {
    pub fn initialized() -> Self {
        Self::Initialized {
            title: None,
            v2: None,
            sliding: None,
            show_logs: false
        }
    }

    pub fn get_v2(&self) -> Option<&Syncv2State> {
        if let Self::Initialized { ref v2, .. } = self {
            v2.as_ref()
        } else {
            None
        }
    }

    pub fn get_v2_mut<'a>(&'a mut self) -> Option<&'a mut Syncv2State> {
        if let Self::Initialized { v2, .. } = self {
            v2.as_mut()
        } else {
            None
        }
    }

    pub fn start_v2(&mut self) {
        if let Self::Initialized { v2, .. } = self {
            if let Some(pre) = v2 {
                warn!("Overwriting previous start from {:#?} taking {:#?}", pre.started(), pre.time_to_first_render());
            }
            *v2 = Some(Syncv2State::new());
        }
    }

    pub fn get_sliding(&self) -> Option<&SlidingSyncState> {
        if let Self::Initialized { ref sliding, .. } = self {
            sliding.as_ref()
        } else {
            None
        }
    }

    pub fn get_sliding_mut<'a>(&'a mut self) -> Option<&'a mut SlidingSyncState> {
        if let Self::Initialized { sliding, .. } = self {
            sliding.as_mut()
        } else {
            None
        }
    }

    pub fn start_sliding(&mut self, view: SlidingSyncView) {
        if let Self::Initialized { sliding, .. } = self {
            if let Some(pre) = sliding {
                warn!("Overwriting previous start from {:#?} taking {:#?}", pre.started(), pre.time_to_first_render());
            }
            *sliding = Some(SlidingSyncState::new(view));
        }
    }

    pub fn is_initialized(&self) -> bool {
        matches!(self, &Self::Initialized { .. })
    }

    pub fn show_logs(&self) -> bool {
        if let Self::Initialized { show_logs, .. } = self {
            *show_logs
        } else {
            false
        }
    }
    pub fn toggle_show_logs(&mut self) {
        if let Self::Initialized { show_logs, .. } = self {
            *show_logs = !*show_logs
        }
    }
    pub fn set_title(&mut self, new_title: Option<String>) {
        if let Self::Initialized { title, .. } = self {
            *title = new_title;
        }
    }
    pub fn title(&self) -> Option<String> {
        if let Self::Initialized { title, .. } = self {
            title.clone()
        } else {
            None
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::Init
    }
}
