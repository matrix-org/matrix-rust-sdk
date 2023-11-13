use matrix_sdk::RoomCall as SdkRoomCall;

#[derive(uniffi::Object)]
pub struct RoomCall {
    pub(crate) inner: SdkRoomCall,
}

#[uniffi::export]
impl RoomCall {
    /// Returns the time when the call was started, i.e. when the first member
    /// joined.
    ///
    /// Returns `None` if there are no members in the call (if the call is
    /// empty/ended).
    pub fn start_time(&self) -> Option<u64> {
        // TODO: We must take into an account that a call could have been started by a
        // participant who is no longer a member of a call.
        self.inner.start_time().map(|t| t.get().into())
    }

    /// Non-expired members of a call.
    pub fn active_members(&self) -> Vec<CallMemberIdentifier> {
        self.inner
            .active_members()
            .map(|id| CallMemberIdentifier {
                user_id: id.user_id.to_string(),
                device_id: id.device_id.to_string(),
            })
            .collect()
    }
}

/// A unique identifier of a call member (participant).
/// Each call member is identified by their user ID and device ID.
#[derive(uniffi::Record)]
pub struct CallMemberIdentifier {
    pub user_id: String,
    pub device_id: String,
}
