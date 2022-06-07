use std::ops::Deref;

use crate::RoomType;

mod common;
mod invited;
mod joined;
mod left;

pub use self::{
    common::{Common, Messages, MessagesOptions},
    invited::Invited,
    joined::Joined,
    left::Left,
};

/// An enum that abstracts over the different states a room can be in.
#[derive(Debug, Clone)]
pub enum Room {
    /// The room in the `join` state.
    Joined(Joined),
    /// The room in the `left` state.
    Left(Left),
    /// The room in the `invited` state.
    Invited(Invited),
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
        match room.room_type() {
            RoomType::Joined => Self::Joined(Joined { inner: room }),
            RoomType::Left => Self::Left(Left { inner: room }),
            RoomType::Invited => Self::Invited(Invited { inner: room }),
        }
    }
}

impl From<Joined> for Room {
    fn from(room: Joined) -> Self {
        let room = (*room).clone();
        match room.room_type() {
            RoomType::Joined => Self::Joined(Joined { inner: room }),
            RoomType::Left => Self::Left(Left { inner: room }),
            RoomType::Invited => Self::Invited(Invited { inner: room }),
        }
    }
}

impl From<Left> for Room {
    fn from(room: Left) -> Self {
        let room = (*room).clone();
        match room.room_type() {
            RoomType::Joined => Self::Joined(Joined { inner: room }),
            RoomType::Left => Self::Left(Left { inner: room }),
            RoomType::Invited => Self::Invited(Invited { inner: room }),
        }
    }
}

impl From<Invited> for Room {
    fn from(room: Invited) -> Self {
        let room = (*room).clone();
        match room.room_type() {
            RoomType::Joined => Self::Joined(Joined { inner: room }),
            RoomType::Left => Self::Left(Left { inner: room }),
            RoomType::Invited => Self::Invited(Invited { inner: room }),
        }
    }
}
