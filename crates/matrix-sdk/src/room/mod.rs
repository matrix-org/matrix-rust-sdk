//! High-level room API

use std::ops::Deref;

use crate::RoomState;

mod common;
mod futures;
mod member;
mod messages;

pub use self::{
    common::{Common, Invite, Receipts},
    futures::SendAttachment,
    member::RoomMember,
    messages::{Messages, MessagesOptions},
};

/// An enum that abstracts over the different states a room can be in.
#[derive(Debug, Clone)]
pub enum Room {
    /// The room in the `join` state.
    Joined(Common),
    /// The room in the `left` state.
    Left(Common),
    /// The room in the `invited` state.
    Invited(Common),
}

impl Deref for Room {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Joined(room) => room,
            Self::Left(room) => room,
            Self::Invited(room) => room,
        }
    }
}

impl From<Common> for Room {
    fn from(room: Common) -> Self {
        match room.state() {
            RoomState::Joined => Self::Joined(room),
            RoomState::Left => Self::Left(room),
            RoomState::Invited => Self::Invited(room),
        }
    }
}
