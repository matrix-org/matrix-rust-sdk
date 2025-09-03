use clap::{Parser, Subcommand};
use crossterm::event::KeyEvent;
use matrix_sdk::Room;
use ratatui::{prelude::*, widgets::*};
use style::palette::tailwind;
use tui_textarea::TextArea;

#[derive(Debug, Parser)]
#[command(name = "multiverse", disable_help_flag = true, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Invite { user_id: String },
    Leave,
    Subscribe,
    Unsubscribe,
}

pub enum MessageOrCommand {
    Message(String),
    Command(Command),
}

/// A widget representing a text input to send messages to a room.
#[derive(Default)]
pub struct Input {
    /// The text area that will keep track of what the user has input.
    textarea: TextArea<'static>,
}

impl Input {
    /// Create a new empty [`Input`] widget.
    pub fn new() -> Self {
        let textarea = Self::text_area();

        Self { textarea }
    }

    /// Receive a key press event and handle it.
    pub fn handle_key_press(&mut self, event: KeyEvent) {
        self.textarea.input(event);
    }

    /// Get the currently input text.
    pub fn get_input(&self) -> Result<MessageOrCommand, clap::Error> {
        let input = self.textarea.lines().join("\n");

        if let Some(input) = input.strip_prefix("/") {
            let arguments = input.split_whitespace();

            // Clap expects the first argument to be the binary name, like when a command is
            // invoked on the command line. Since we aren't a command line, but
            // still find clap a neat command parser, let's give it what it
            // expects so we can parse our commands.
            Cli::try_parse_from(std::iter::once("multiverse").chain(arguments))
                .map(|cli| MessageOrCommand::Command(cli.command))
        } else {
            Ok(MessageOrCommand::Message(input))
        }
    }

    /// Is the input area empty, returns false if the user hasn't input anything
    /// yet.
    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    /// Clear the text from the input area.
    pub fn clear(&mut self) {
        self.textarea = Self::text_area();
    }

    fn text_area() -> TextArea<'static> {
        let mut area = TextArea::default();
        area.set_placeholder_style(Style::new().fg(tailwind::GRAY.c50));

        area
    }
}

impl<'a> StatefulWidget for &'a mut Input {
    type State = Option<&'a Room>;

    fn render(self, area: Rect, buf: &mut Buffer, room: &mut Self::State)
    where
        Self: Sized,
    {
        // Set the placeholder text depending on the encryption state of the room.
        //
        // We assume that the encryption state is synced because the RoomListService
        // sets it as required state.
        if let Some(room) = room.as_deref() {
            let is_encrypted = room.encryption_state().is_encrypted();

            if is_encrypted {
                self.textarea.set_placeholder_text("(Send an encrypted message)");
            } else {
                self.textarea.set_placeholder_text("(Send an unencrypted message)");
            }
        } else {
            self.textarea.set_placeholder_text("(No room selected)");
        }

        // Let's first create a block to set the background color.
        let input_block = Block::new().borders(Borders::NONE).bg(tailwind::BLUE.c400);

        // Now we set the block and we render the textarea.
        self.textarea.set_block(input_block);
        self.textarea.render(area, buf);
    }
}
