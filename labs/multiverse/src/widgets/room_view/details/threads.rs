use itertools::Itertools;
use matrix_sdk::{
    deserialized_responses::TimelineEvent, event_cache::ThreadSummary, ruma::OwnedRoomId,
};
use matrix_sdk_ui::room_list_service::Room;
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Wrap},
};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::TEXT_COLOR;

/// Small state machine representing the loading of the thread list for a given
/// room.
#[derive(Default)]
enum ThreadListLoaderState {
    /// The thread loaded is inactive; it hasn't been initialized yet, because
    /// it hasn't seen a room yet, or it's in the process of being reset.
    #[default]
    Empty,

    /// We've started a task to load threads, but it hasn't finished yet.
    Loading(
        Option<JoinHandle<Result<Vec<TimelineEvent>, matrix_sdk::event_cache::EventCacheError>>>,
    ),

    /// We've successfully loaded the list of threads, and this is the array of
    /// the thread roots.
    Loaded(Vec<TimelineEvent>),

    /// We've ran into an error while loading the list of threads.
    Error(String),
}

impl ThreadListLoaderState {
    fn reset(&mut self) {
        match self {
            ThreadListLoaderState::Loading(task) => {
                if let Some(task) = task.take() {
                    // Cancel the task if it's still running.
                    task.abort();
                }
            }
            _ => {}
        }

        *self = Self::default();
    }
}

#[derive(Default)]
pub struct ThreadListLoader {
    state: ThreadListLoaderState,
    room: Option<OwnedRoomId>,
}

impl ThreadListLoader {
    /// Start the task loading the threads root for this room, if it hasn't
    /// started yet.
    ///
    /// If the room id is different from the previous one we had, restart the
    /// local state.
    pub fn init_if_needed(&mut self, room: Option<&Room>) {
        if let Some(room) = room {
            // If we've switched room, reinitialize the thread loader.
            if self.room.as_deref() != Some(room.room_id()) {
                self.state.reset();
                self.room = Some(room.room_id().to_owned());
            }

            // If the thread loader was inactive, kick off a task to initialize it.
            if let ThreadListLoaderState::Empty = self.state {
                let room = room.clone();
                let task = tokio::task::spawn(async move {
                    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
                    room_event_cache.list_all_threads().await
                });
                self.state = ThreadListLoaderState::Loading(Some(task))
            }
        }
    }
}

#[derive(Default)]
pub struct ThreadsView;

impl ThreadsView {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StatefulWidget for &mut ThreadsView {
    type State = ThreadListLoader;

    fn render(self, area: Rect, buf: &mut Buffer, loader: &mut Self::State)
    where
        Self: Sized,
    {
        let events = {
            match &mut loader.state {
                ThreadListLoaderState::Empty => {
                    Paragraph::new("Waiting for the task to initialize…")
                        .fg(TEXT_COLOR)
                        .render(area, buf);
                    return;
                }

                ThreadListLoaderState::Loading(task) => {
                    if task.as_ref().is_some_and(|t| t.is_finished()) {
                        // The task is finished: awaiting it will immediately complete.
                        let task = task.take().unwrap();
                        let events =
                            tokio::task::block_in_place(|| Handle::current().block_on(task));

                        let events = events.expect("list_all_threads task failed to join");

                        match events {
                            Ok(events) => {
                                loader.state = ThreadListLoaderState::Loaded(events.clone());
                            }
                            Err(e) => {
                                loader.state = ThreadListLoaderState::Error(format!(
                                    "Error listing threads: {}",
                                    e
                                ));
                                return;
                            }
                        }
                    }

                    Paragraph::new("Loading threads…").fg(TEXT_COLOR).render(area, buf);
                    return;
                }

                ThreadListLoaderState::Error(ref error) => {
                    Paragraph::new(error.as_str())
                        .fg(TEXT_COLOR)
                        .wrap(Wrap { trim: false })
                        .render(area, buf);
                    return;
                }

                ThreadListLoaderState::Loaded(ref events) => events.clone(),
            }
        };

        if events.is_empty() {
            Paragraph::new("No threads found").fg(TEXT_COLOR).render(area, buf);
            return;
        }

        let separator = Line::from("\n");
        let events = events
            .into_iter()
            .map(|ev| {
                let (summary, _latest_event) = ThreadSummary::extract_from_bundled(ev.raw())
                    .expect("all threads must have a summary!");
                format!(
                    "- Thread: root={}, count={}, latest_event_id={}",
                    ev.event_id().unwrap(),
                    summary.count,
                    summary.latest_event,
                )
            })
            .map(Line::from);

        let events = Itertools::intersperse(events, separator);
        let lines: Vec<_> = [Line::from("")].into_iter().chain(events).collect();

        Paragraph::new(lines).fg(TEXT_COLOR).wrap(Wrap { trim: false }).render(area, buf);
    }
}
