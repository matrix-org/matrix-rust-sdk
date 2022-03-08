use std::time::{Instant, Duration};

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

#[derive(Clone)]
pub enum AppState {
    Init,
    Initialized {
        title: Option<String>,
        v2: Option<Syncv2State>
    },
}

impl AppState {
    pub fn initialized() -> Self {
        Self::Initialized {
            title: None,
            v2: None,
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

    pub fn is_initialized(&self) -> bool {
        matches!(self, &Self::Initialized { .. })
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
