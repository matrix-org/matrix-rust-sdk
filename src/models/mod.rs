use crate::events::{
    call::{
        answer::AnswerEvent, candidates::CandidatesEvent, hangup::HangupEvent, invite::InviteEvent,
    },
    direct::DirectEvent,
    dummy::DummyEvent,
    forwarded_room_key::ForwardedRoomKeyEvent,
    fully_read::FullyReadEvent,
    ignored_user_list::IgnoredUserListEvent,
    key::verification::{
        accept::AcceptEvent, cancel::CancelEvent, key::KeyEvent, mac::MacEvent,
        request::RequestEvent, start::StartEvent,
    },
    presence::PresenceEvent,
    push_rules::PushRulesEvent,
    receipt::ReceiptEvent,
    room::{
        aliases::AliasesEvent,
        avatar::AvatarEvent,
        canonical_alias::CanonicalAliasEvent,
        create::CreateEvent,
        encrypted::EncryptedEvent,
        encryption::EncryptionEvent,
        guest_access::GuestAccessEvent,
        history_visibility::HistoryVisibilityEvent,
        join_rules::JoinRulesEvent,
        member::MemberEvent,
        message::{feedback::FeedbackEvent, MessageEvent},
        name::NameEvent,
        pinned_events::PinnedEventsEvent,
        power_levels::PowerLevelsEvent,
        redaction::RedactionEvent,
        server_acl::ServerAclEvent,
        third_party_invite::ThirdPartyInviteEvent,
        tombstone::TombstoneEvent,
        topic::TopicEvent,
    },
    room_key::RoomKeyEvent,
    room_key_request::RoomKeyRequestEvent,
    sticker::StickerEvent,
    tag::TagEvent,
    typing::TypingEvent,
    CustomEvent, CustomRoomEvent, CustomStateEvent,
};

mod room_member;
mod room_state;
mod room;
mod user;

pub use room::{Room, RoomName};
pub use room_member::RoomMember;
pub use user::User;

pub type Token = String;
pub type RoomId = String;
pub type UserId = String;

pub enum EventWrapper<'ev> {
    /// m.call.answer
    CallAnswer(&'ev AnswerEvent),

    /// m.call.candidates
    CallCandidates(&'ev CandidatesEvent),

    /// m.call.hangup
    CallHangup(&'ev HangupEvent),

    /// m.call.invite
    CallInvite(&'ev InviteEvent),

    /// m.direct
    Direct(&'ev DirectEvent),

    /// m.dummy
    Dummy(&'ev DummyEvent),

    /// m.forwarded_room_key
    ForwardedRoomKey(&'ev ForwardedRoomKeyEvent),

    /// m.fully_read
    FullyRead(&'ev FullyReadEvent),

    /// m.ignored_user_list
    IgnoredUserList(&'ev IgnoredUserListEvent),

    /// m.key.verification.accept
    KeyVerificationAccept(&'ev AcceptEvent),

    /// m.key.verification.cancel
    KeyVerificationCancel(&'ev CancelEvent),

    /// m.key.verification.key
    KeyVerificationKey(&'ev KeyEvent),

    /// m.key.verification.mac
    KeyVerificationMac(&'ev MacEvent),

    /// m.key.verification.request
    KeyVerificationRequest(&'ev RequestEvent),

    /// m.key.verification.start
    KeyVerificationStart(&'ev StartEvent),

    /// m.presence
    Presence(&'ev PresenceEvent),

    /// m.push_rules
    PushRules(&'ev PushRulesEvent),

    /// m.receipt
    Receipt(&'ev ReceiptEvent),

    /// m.room.aliases
    RoomAliases(&'ev AliasesEvent),

    /// m.room.avatar
    RoomAvatar(&'ev AvatarEvent),

    /// m.room.canonical_alias
    RoomCanonicalAlias(&'ev CanonicalAliasEvent),

    /// m.room.create
    RoomCreate(&'ev CreateEvent),

    /// m.room.encrypted
    RoomEncrypted(&'ev EncryptedEvent),

    /// m.room.encryption
    RoomEncryption(&'ev EncryptionEvent),

    /// m.room.guest_access
    RoomGuestAccess(&'ev GuestAccessEvent),

    /// m.room.history_visibility
    RoomHistoryVisibility(&'ev HistoryVisibilityEvent),

    /// m.room.join_rules
    RoomJoinRules(&'ev JoinRulesEvent),

    /// m.room.member
    RoomMember(&'ev MemberEvent),

    /// m.room.message
    RoomMessage(&'ev MessageEvent),

    /// m.room.message.feedback
    RoomMessageFeedback(&'ev FeedbackEvent),

    /// m.room.name
    RoomName(&'ev NameEvent),

    /// m.room.pinned_events
    RoomPinnedEvents(&'ev PinnedEventsEvent),

    /// m.room.power_levels
    RoomPowerLevels(&'ev PowerLevelsEvent),

    /// m.room.redaction
    RoomRedaction(&'ev RedactionEvent),

    /// m.room.server_acl
    RoomServerAcl(&'ev ServerAclEvent),

    /// m.room.third_party_invite
    RoomThirdPartyInvite(&'ev ThirdPartyInviteEvent),

    /// m.room.tombstone
    RoomTombstone(&'ev TombstoneEvent),

    /// m.room.topic
    RoomTopic(&'ev TopicEvent),

    /// m.room_key
    RoomKey(&'ev RoomKeyEvent),

    /// m.room_key_request
    RoomKeyRequest(&'ev RoomKeyRequestEvent),

    /// m.sticker
    Sticker(&'ev StickerEvent),

    /// m.tag
    Tag(&'ev TagEvent),

    /// m.typing
    Typing(&'ev TypingEvent),

    /// Any basic event that is not part of the specification.
    Custom(&'ev CustomEvent),

    /// Any room event that is not part of the specification.
    CustomRoom(&'ev CustomRoomEvent),

    /// Any state event that is not part of the specification.
    CustomState(&'ev CustomStateEvent),
}
