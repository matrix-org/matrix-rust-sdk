use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::FutureExt as _;
use matrix_sdk::{
    Client,
    encryption::{CrossSigningResetAuthType, recovery::RecoveryError},
    reqwest::Url,
    ruma::api::client::uiaa::{AuthData, Password},
};
use matrix_sdk_common::executor::spawn;
use ratatui::{
    prelude::*,
    widgets::{Block, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::{
    sync::{
        mpsc::{UnboundedSender, unbounded_channel},
        oneshot,
    },
    task::JoinHandle,
};
use tui_textarea::TextArea;

use super::ShouldExit;
use crate::widgets::{Hyperlink, recovery::create_centered_throbber_area};

#[derive(Debug)]
enum ResetState {
    Waiting { receiver: oneshot::Receiver<ResetMessage> },
    ResettingOauth { approval_url: Url },
    InputtingMatrixAuthInfo { sender: UnboundedSender<String>, text_area: TextArea<'static> },
    ResettingMatrixAuth,
    Done,
}

#[derive(Debug)]
enum ResetMessage {
    Oauth { approval_url: Url },
    MatrixAuth { password_sender: UnboundedSender<String> },
}

#[derive(Debug)]
pub struct RecoveringView {
    client: Client,
    mode: Mode,
}

#[derive(Debug)]
enum Mode {
    Recovering {
        recovery_task: JoinHandle<Result<(), RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Resetting {
        reset_state: ResetState,
        reset_task: JoinHandle<Result<(), RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Inputting {
        recovery_text_area: TextArea<'static>,
    },
    Done {
        result: Result<(), RecoveryError>,
    },
}

impl RecoveringView {
    pub fn new(client: Client) -> Self {
        let mut recovery_text_area = TextArea::default();

        recovery_text_area.set_cursor_line_style(Style::default());
        recovery_text_area.set_mask_char('\u{2022}'); //U+2022 BULLET (•)
        recovery_text_area.set_placeholder_text("To enable recovery enter the recover key");

        recovery_text_area.set_style(Style::default().fg(Color::LightGreen));
        recovery_text_area.set_block(Block::default());

        Self { client, mode: Mode::Inputting { recovery_text_area } }
    }

    fn update(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Recovering { recovery_task, .. } => {
                if recovery_task.is_finished() {
                    let result = recovery_task
                        .now_or_never()
                        .expect("The task should have finished, we checked it")
                        .expect("The recovery enabling task should neve panic");
                    self.mode = Done { result };
                }
            }
            Resetting { reset_state, reset_task, .. } => match reset_state {
                ResetState::Waiting { receiver } => {
                    match receiver.try_recv() {
                        Ok(ResetMessage::Oauth { approval_url }) => {
                            *reset_state = ResetState::ResettingOauth { approval_url };
                        }
                        Ok(ResetMessage::MatrixAuth { password_sender }) => {
                            let mut text_area = TextArea::default();

                            text_area.set_cursor_line_style(Style::default());
                            text_area.set_mask_char('\u{2022}'); //U+2022 BULLET (•)
                            text_area
                                .set_placeholder_text("To reset your identity enter your password");

                            text_area.set_style(Style::default().fg(Color::LightGreen));
                            text_area.set_block(Block::default());

                            *reset_state = ResetState::InputtingMatrixAuthInfo {
                                sender: password_sender,
                                text_area,
                            };
                        }
                        _ => {}
                    }
                }
                ResetState::InputtingMatrixAuthInfo { .. } | ResetState::Done => {}
                ResetState::ResettingOauth { .. } | ResetState::ResettingMatrixAuth => {
                    if reset_task.is_finished() {
                        *reset_state = ResetState::Done;
                    }
                }
            },
            Inputting { .. } | Done { .. } => {}
        }
    }

    pub fn on_tick(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Recovering { throbber_state, .. } => throbber_state.calc_next(),
            Resetting { throbber_state, .. } => throbber_state.calc_next(),
            Inputting { .. } | Done { .. } => {}
        }
    }

    pub fn is_idle(&self) -> bool {
        match self.mode {
            Mode::Recovering { .. } | Mode::Resetting { .. } | Mode::Done { .. } => false,
            Mode::Inputting { .. } => true,
        }
    }

    fn handle_identity_reset(&mut self) {
        let client = self.client.clone();
        let (sender, receiver) = oneshot::channel();

        let user_id = client
            .user_id()
            .expect("We should have access to our user ID if we're resetting our identity")
            .to_owned();

        let reset_task = spawn(async move {
            let handle = client.encryption().recovery().reset_identity().await?;

            if let Some(handle) = handle {
                match handle.auth_type() {
                    CrossSigningResetAuthType::Uiaa(_) => {
                        let (password_sender, mut password_receiver) = unbounded_channel();
                        let _ = sender.send(ResetMessage::MatrixAuth { password_sender });

                        let password = password_receiver
                            .recv()
                            .await
                            .expect("The sender should not have been closed");

                        handle
                            .reset(Some(AuthData::Password(Password::new(
                                user_id.into(),
                                password,
                            ))))
                            .await
                    }
                    CrossSigningResetAuthType::OAuth(oauth_cross_signing_reset_info) => {
                        sender
                            .send(ResetMessage::Oauth {
                                approval_url: oauth_cross_signing_reset_info.approval_url.clone(),
                            })
                            .expect("");
                        handle.reset(None).await
                    }
                }
            } else {
                Ok(())
            }
        });

        let reset_state = ResetState::Waiting { receiver };

        self.mode =
            Mode::Resetting { reset_state, reset_task, throbber_state: ThrobberState::default() };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ShouldExit {
        use KeyCode::*;
        use Mode::*;
        use ShouldExit::*;

        match &mut self.mode {
            Recovering { .. } => No,
            Resetting { reset_state, .. } => match reset_state {
                ResetState::Waiting { .. }
                | ResetState::ResettingOauth { .. }
                | ResetState::ResettingMatrixAuth => match (key.modifiers, key.code) {
                    (_, Esc) => {
                        *self = Self::new(self.client.clone());
                        No
                    }
                    _ => No,
                },
                ResetState::InputtingMatrixAuthInfo { sender, text_area } => {
                    match (key.modifiers, key.code) {
                        (_, Enter) => {
                            let password = text_area.lines().join("");
                            sender
                                .send(password)
                                .expect("The task should still wait for the password");

                            *reset_state = ResetState::ResettingMatrixAuth;

                            No
                        }
                        _ => {
                            text_area.input(key);
                            No
                        }
                    }
                }

                ResetState::Done => OnlySubScreen,
            },

            Inputting { recovery_text_area } => {
                match (key.modifiers, key.code) {
                    (KeyModifiers::CONTROL, Char('r')) => {
                        self.handle_identity_reset();
                        No
                    }
                    (_, Esc) => Yes,
                    (_, Enter) => {
                        // We expect a single line since pressing enter gets us here, still, let's
                        // just join all the lines into a single one.
                        let recovery_key = recovery_text_area.lines().join("");
                        let client = self.client.clone();

                        let recovery_task = spawn(async move {
                            client.encryption().recovery().recover(recovery_key.trim()).await
                        });

                        self.mode =
                            Recovering { recovery_task, throbber_state: ThrobberState::default() };

                        No
                    }
                    _ => {
                        recovery_text_area.input(key);

                        No
                    }
                }
            }
            Done { .. } => OnlySubScreen,
        }
    }
}

impl Widget for &mut RecoveringView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        use Mode::*;

        self.update();

        match &mut self.mode {
            Recovering { throbber_state, .. } => {
                let throbber = Throbber::default()
                    .label("Recovering")
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                let centered_area = create_centered_throbber_area(area);
                StatefulWidget::render(throbber, centered_area, buf, throbber_state);
            }

            Resetting { throbber_state, reset_state, .. } => match reset_state {
                ResetState::InputtingMatrixAuthInfo { text_area, .. } => {
                    let [left, right] =
                        Layout::horizontal([Constraint::Length(14), Constraint::Length(50)])
                            .areas(area);

                    Paragraph::new("Password:").render(left, buf);
                    text_area.render(right, buf);
                }
                ResetState::ResettingOauth { approval_url } => {
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .margin(1)
                        .constraints([
                            Constraint::Fill(1),
                            Constraint::Length(65),
                            Constraint::Fill(1),
                        ])
                        .split(area);

                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .margin(1)
                        .constraints([
                            Constraint::Fill(1),
                            Constraint::Length(1),
                            Constraint::Fill(1),
                        ])
                        .split(chunks[1]);

                    let centered_area = chunks[1];

                    let [left, right] =
                        Layout::horizontal([Constraint::Length(38), Constraint::Length(22)])
                            .areas(centered_area);

                    let hyperlink = Hyperlink::new(
                        Text::from("account management URL").blue(),
                        approval_url.to_string(),
                    );

                    Text::from("To finish the reset approve it at the ").render(left, buf);
                    hyperlink.render(right, buf);
                }
                ResetState::Waiting { .. } | ResetState::ResettingMatrixAuth => {
                    let throbber = Throbber::default()
                        .label("Resetting your identity")
                        .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                    let centered_area = create_centered_throbber_area(area);
                    StatefulWidget::render(throbber, centered_area, buf, throbber_state);
                }
                ResetState::Done => {
                    let constraints =
                        [Constraint::Fill(1), Constraint::Min(3), Constraint::Fill(1)];
                    let [_top, middle, _bottom] = Layout::vertical(constraints).areas(area);

                    Paragraph::new("Done resetting\n\nPress any key to continue")
                        .centered()
                        .render(middle, buf);
                }
            },

            Inputting { recovery_text_area } => {
                let [left, right] =
                    Layout::horizontal([Constraint::Length(14), Constraint::Length(50)])
                        .areas(area);

                Paragraph::new("Recovery key: ").render(left, buf);
                recovery_text_area.render(right, buf);
            }

            Done { result } => {
                let constraints = [Constraint::Fill(1), Constraint::Min(3), Constraint::Fill(1)];
                let [_top, middle, _bottom] = Layout::vertical(constraints).areas(area);

                match result {
                    Ok(_) => {
                        Paragraph::new("Done recovering\n\nPress any key to continue")
                            .centered()
                            .render(middle, buf);
                    }
                    Err(error) => {
                        Paragraph::new(format!(
                            "Error recovering: {error:?}\n\nPress any key to continue"
                        ))
                        .centered()
                        .render(middle, buf);
                    }
                }
            }
        }
    }
}
