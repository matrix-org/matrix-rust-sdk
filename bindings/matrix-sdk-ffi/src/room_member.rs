use matrix_sdk::room::RoomMember as SdkRoomMember;

#[derive(Clone)]
pub enum MembershipState {
    /// The user is banned.
    Ban,

    /// The user has been invited.
    Invite,

    /// The user has joined.
    Join,

    /// The user has requested to join.
    Knock,

    /// The user has left.
    Leave,
}

impl From<matrix_sdk::ruma::events::room::member::MembershipState> for MembershipState {
    fn from(m: matrix_sdk::ruma::events::room::member::MembershipState) -> Self {
        match m {
            matrix_sdk::ruma::events::room::member::MembershipState::Ban => MembershipState::Ban,
            matrix_sdk::ruma::events::room::member::MembershipState::Invite => {
                MembershipState::Invite
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Join => MembershipState::Join,
            matrix_sdk::ruma::events::room::member::MembershipState::Knock => {
                MembershipState::Knock
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Leave => {
                MembershipState::Leave
            }
            _ => todo!(
                "Handle Custom case: https://github.com/matrix-org/matrix-rust-sdk/issues/1254"
            ),
        }
    }
}

pub struct RoomMember {
    inner: SdkRoomMember,
}

#[uniffi::export]
impl RoomMember {
    fn user_id(&self) -> String {
        self.inner.user_id().to_string()
    }

    fn display_name(&self) -> Option<String> {
        self.inner.display_name().map(|d| d.to_owned())
    }

    fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(ToString::to_string)
    }

    fn membership(&self) -> MembershipState {
        self.inner.membership().to_owned().into()
    }

    fn is_name_ambiguous(&self) -> bool {
        self.inner.name_ambiguous()
    }

    fn power_level(&self) -> i64 {
        self.inner.power_level()
    }

    fn normalized_power_level(&self) -> i64 {
        self.inner.normalized_power_level()
    }

    fn is_ignored(&self) -> bool {
        self.inner.is_ignored()
    }
}

impl RoomMember {
    pub fn new(room_member: SdkRoomMember) -> Self {
        RoomMember { inner: room_member }
    }
}
