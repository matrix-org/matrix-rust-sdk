//! High-level room API

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
