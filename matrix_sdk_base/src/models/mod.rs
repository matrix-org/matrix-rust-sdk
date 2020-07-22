#[cfg(feature = "messages")]
#[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
mod message;
mod room;
mod room_member;

#[cfg(feature = "messages")]
#[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
pub use message::{MessageQueue, MessageWrapper, PossiblyRedactedExt};
pub use room::{Room, RoomName};
pub use room_member::RoomMember;
