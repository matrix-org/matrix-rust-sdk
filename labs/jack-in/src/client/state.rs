use std::time::{Instant, Duration};
use matrix_sdk::{Client, SlidingSyncView, SlidingSyncRoom};
use tuirealm::tui::widgets::TableState;
use std::collections::btree_map::BTreeMap;
use std::sync::Arc;
use matrix_sdk_common::ruma::{
    RoomId, 
    events::AnyRoomEvent
};
use futures_signals::{
    signal::Mutable,
    signal_vec::MutableVec,
};

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
    /// the current list selector for the room
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    current_rooms_count: Option<u32>,
    total_rooms_count: Option<u32>,
    pub selected_room: Mutable<Option<Box<matrix_sdk::ruma::RoomId>>>
}

impl std::cmp::PartialOrd for SlidingSyncState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        None
    }
}

impl std::cmp::Ord for SlidingSyncState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl std::cmp::Eq for SlidingSyncState { }

impl std::cmp::PartialEq for SlidingSyncState {
    fn eq(&self, other: &SlidingSyncState) -> bool {
        false
    }

    fn ne(&self, other: &SlidingSyncState) -> bool { 
        false
    }
}


impl SlidingSyncState {
    pub fn new(view: SlidingSyncView) -> Self {
        Self {
            started: Instant::now(),
            view,
            first_render: None,
            full_sync: None,
            selected_room: Mutable::new(None),
            current_rooms_count: None,
            total_rooms_count: None,
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn select_room(&self, r: Option<Box<RoomId>>) {
        self.selected_room.replace(r);
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

    pub fn total_rooms_count(&self) -> Option<u32> {
        self.view.rooms_count.get()
    }

    pub fn set_first_render_now(&mut self) {
        self.first_render = Some(self.started.elapsed())
    }

    pub fn view(&self) -> &SlidingSyncView {
        &self.view
    }

    pub fn set_full_sync_now(&mut self) {
        self.full_sync = Some(self.started.elapsed())
    }
}

// #[derive(Clone)]
// pub enum AppState {
//     Init,
//     Initialized {
//         title: Option<String>,
//         v2: Option<Syncv2State>,
//         sliding: Option<SlidingSyncState>,
//         show_logs: bool
//     },
// }

// impl AppState {
//     pub fn initialized() -> Self {
//         Self::Initialized {
//             title: None,
//             v2: None,
//             sliding: None,
//             show_logs: false
//         }
//     }

//     pub fn get_v2(&self) -> Option<&Syncv2State> {
//         if let Self::Initialized { ref v2, .. } = self {
//             v2.as_ref()
//         } else {
//             None
//         }
//     }

//     pub fn get_v2_mut<'a>(&'a mut self) -> Option<&'a mut Syncv2State> {
//         if let Self::Initialized { v2, .. } = self {
//             v2.as_mut()
//         } else {
//             None
//         }
//     }

//     pub fn start_v2(&mut self) {
//         if let Self::Initialized { v2, .. } = self {
//             if let Some(pre) = v2 {
//                 warn!("Overwriting previous start from {:#?} taking {:#?}", pre.started(), pre.time_to_first_render());
//             }
//             *v2 = Some(Syncv2State::new());
//         }
//     }

//     pub fn get_sliding(&self) -> Option<&SlidingSyncState> {
//         if let Self::Initialized { ref sliding, .. } = self {
//             sliding.as_ref()
//         } else {
//             None
//         }
//     }

//     pub fn get_sliding_mut<'a>(&'a mut self) -> Option<&'a mut SlidingSyncState> {
//         if let Self::Initialized { sliding, .. } = self {
//             sliding.as_mut()
//         } else {
//             None
//         }
//     }

//     pub fn start_sliding(&mut self, view: SlidingSyncView) {
//         if let Self::Initialized { sliding, .. } = self {
//             if let Some(pre) = sliding {
//                 warn!("Overwriting previous start from {:#?} taking {:#?}", pre.started(), pre.time_to_first_render());
//             }
//             *sliding = Some(SlidingSyncState::new(view));
//         }
//     }

//     pub fn is_initialized(&self) -> bool {
//         matches!(self, &Self::Initialized { .. })
//     }

//     pub fn show_logs(&self) -> bool {
//         if let Self::Initialized { show_logs, .. } = self {
//             *show_logs
//         } else {
//             false
//         }
//     }
//     pub fn toggle_show_logs(&mut self) {
//         if let Self::Initialized { show_logs, .. } = self {
//             *show_logs = !*show_logs
//         }
//     }
//     pub fn set_title(&mut self, new_title: Option<String>) {
//         if let Self::Initialized { title, .. } = self {
//             *title = new_title;
//         }
//     }
//     pub fn title(&self) -> Option<String> {
//         if let Self::Initialized { title, .. } = self {
//             title.clone()
//         } else {
//             None
//         }
//     }
// }

// impl Default for AppState {
//     fn default() -> Self {
//         Self::Init
//     }
// }
