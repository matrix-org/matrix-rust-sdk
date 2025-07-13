use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use futures_util::FutureExt;
use matrix_sdk::{Room, RoomState, room::Invite};
use ratatui::{prelude::*, widgets::*};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::{spawn, task::JoinHandle};
use tui_framework_experiment::{Button, button};

use crate::widgets::recovery::create_centered_throbber_area;

enum Mode {
    Loading { task: JoinHandle<Result<Invite, matrix_sdk::Error>> },
    Joining { task: JoinHandle<Result<(), matrix_sdk::Error>> },
    Leaving { task: JoinHandle<Result<(), matrix_sdk::Error>> },
    Loaded { invite_details: Invite },
    Done,
}

impl Drop for Mode {
    fn drop(&mut self) {
        match self {
            Mode::Loading { task } => task.abort(),
            Mode::Joining { task } => task.abort(),
            Mode::Leaving { task } => task.abort(),
            Mode::Loaded { .. } => {}
            Mode::Done => {}
        }
    }
}

enum FocusedButton {
    Accept = 0,
    Reject = 1,
}

impl TryFrom<usize> for FocusedButton {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FocusedButton::Accept),
            1 => Ok(FocusedButton::Reject),
            _ => Err(()),
        }
    }
}

pub struct InvitedRoomView {
    mode: Mode,
    room: Room,
    buttons: Buttons,
}

struct Buttons {
    areas: Vec<Rect>,
    focused_button: FocusedButton,
    accept: Button<'static>,
    reject: Button<'static>,
}

impl Buttons {
    fn focused_button_mut(&mut self) -> &mut Button<'static> {
        match self.focused_button {
            FocusedButton::Accept => &mut self.accept,
            FocusedButton::Reject => &mut self.reject,
        }
    }

    fn focus_next_button(&mut self) {
        self.focused_button_mut().normal();

        match self.focused_button {
            FocusedButton::Accept => self.focused_button = FocusedButton::Reject,
            FocusedButton::Reject => self.focused_button = FocusedButton::Accept,
        }

        self.focused_button_mut().select();
    }

    fn release(&mut self) {
        self.focused_button_mut().select();
    }

    fn click(&mut self, column: u16, row: u16) -> bool {
        for (i, area) in self.areas.iter().enumerate() {
            let area_contains_click = area.left() <= column
                && column < area.right()
                && area.top() <= row
                && row < area.bottom();

            if area_contains_click {
                match i.try_into() {
                    Ok(FocusedButton::Accept) => {
                        self.release();
                        self.accept.toggle_press();
                        self.focused_button = FocusedButton::Accept;
                        return true;
                    }
                    Ok(FocusedButton::Reject) => {
                        self.release();
                        self.reject.toggle_press();
                        self.focused_button = FocusedButton::Accept;
                        return true;
                    }
                    _ => {}
                }

                break;
            }
        }

        false
    }
}

impl InvitedRoomView {
    pub(super) fn new(room: Room) -> Self {
        let task = spawn({
            let room = room.clone();
            async move { room.invite_details().await }
        });

        let mut accept = Button::new("Accept").with_theme(button::themes::GREEN);
        accept.select();

        let mode = Mode::Loading { task };
        let buttons = Buttons {
            focused_button: FocusedButton::Accept,
            accept,
            reject: Button::new("Reject").with_theme(button::themes::RED),
            areas: Vec::new(),
        };

        Self { mode, room, buttons }
    }

    fn join_or_leave(&mut self) {
        let room = self.room.clone();

        let mode = match self.buttons.focused_button {
            FocusedButton::Accept => {
                Mode::Joining { task: spawn(async move { room.join().await }) }
            }
            FocusedButton::Reject => {
                Mode::Leaving { task: spawn(async move { room.leave().await }) }
            }
        };

        self.mode = mode;
    }

    pub fn handle_event(&mut self, event: Event) {
        use KeyCode::*;

        match event {
            Event::Key(KeyEvent { code: Char('j') | Left, .. }) => self.buttons.focus_next_button(),
            Event::Key(KeyEvent { code: Char('k') | Right, .. }) => {
                self.buttons.focus_next_button()
            }
            Event::Key(KeyEvent { code: Char(' ') | Enter, .. }) => {
                self.buttons.focused_button_mut().toggle_press();
                self.join_or_leave()
            }

            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                if self.buttons.click(column, row) {
                    self.join_or_leave()
                }
            }

            Event::Mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), .. }) => {
                self.buttons.release();
            }

            _ => {}
        }
    }

    fn update(&mut self) {
        if !matches!(self.room.state(), RoomState::Invited) {
            match &mut self.mode {
                // Don't go into the `Done` mode before the task doing the join finishes. This is
                // especially important for the shared room history feature, since we do a bunch of
                // work after the `/join` request is sent out to import the historic room keys.
                //
                // This prevents the task from being aborted because switching to the joined room
                // view is decided by the `should_switch()` function.
                Mode::Joining { task } => {
                    if task.is_finished() {
                        self.mode = Mode::Done
                    }
                }
                Mode::Loading { .. } | Mode::Leaving { .. } | Mode::Loaded { .. } | Mode::Done => {
                    self.mode = Mode::Done
                }
            }
        } else {
            match &mut self.mode {
                Mode::Loading { task } => {
                    if task.is_finished() {
                        let invite_details = task
                            .now_or_never()
                            .expect("We checked that the task has finished")
                            .expect("The task shouldn't ever panic")
                            .expect("We should be able to load the invite details from storage");
                        self.mode = Mode::Loaded { invite_details };
                    }
                }
                Mode::Joining { .. } => {}
                Mode::Leaving { .. } => {}
                Mode::Loaded { .. } => {}
                Mode::Done => {}
            }
        }
    }

    pub fn should_switch(&self) -> bool {
        matches!(self.mode, Mode::Done)
    }
}

impl Widget for &mut InvitedRoomView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        self.update();

        let mut create_throbber = |title| {
            let centered = create_centered_throbber_area(area);
            let mut state = ThrobberState::default();
            state.calc_step(0);

            let throbber = Throbber::default()
                .label(title)
                .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);

            StatefulWidget::render(throbber, centered, buf, &mut state);
        };

        match &self.mode {
            Mode::Loading { .. } => create_throbber("Loading"),
            Mode::Leaving { .. } => create_throbber("Rejecting"),
            Mode::Joining { .. } | Mode::Done => create_throbber("Joining"),
            Mode::Loaded { invite_details } => {
                let text = if let Some(inviter) = &invite_details.inviter {
                    let display_name =
                        inviter.display_name().unwrap_or_else(|| inviter.user_id().as_str());

                    format!("{display_name} has invited you to this room")
                } else {
                    "You have been invited to this room".to_owned()
                };

                let [_, middle_area, _] = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Fill(1), Constraint::Length(5), Constraint::Fill(1)])
                    .areas(area);

                let vertical = Layout::vertical([Constraint::Length(2), Constraint::Length(5)]);
                let [label_area, button_area] = vertical.areas(middle_area);

                let paragraph = Paragraph::new(text).centered();
                paragraph.render(label_area, buf);

                let [_, left_button, _, right_button, _] = Layout::horizontal([
                    Constraint::Fill(1),
                    Constraint::Length(20),
                    Constraint::Length(1),
                    Constraint::Length(20),
                    Constraint::Fill(1),
                ])
                .areas(button_area);

                self.buttons.areas = vec![left_button, right_button];

                self.buttons.accept.render(left_button, buf);
                self.buttons.reject.render(right_button, buf);
            }
        }
    }
}
