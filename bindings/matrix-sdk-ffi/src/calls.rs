use matrix_sdk::RoomCall as SdkRoomCall;

#[derive(uniffi::Object)]
pub struct RoomCall {
    pub(crate) inner: SdkRoomCall,
}

#[uniffi::export]
impl RoomCall {
    pub fn start_time(&self) -> u64 {
        self.inner.start_time().get().into()
    }

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
