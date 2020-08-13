#[cfg(feature = "messages")]
mod message;
mod room;
mod room_member;

#[cfg(feature = "messages")]
#[cfg_attr(feature = "docs", doc(cfg(messages)))]
pub use message::{MessageQueue, PossiblyRedactedExt};
pub use room::{Room, RoomName};
pub use room_member::RoomMember;
