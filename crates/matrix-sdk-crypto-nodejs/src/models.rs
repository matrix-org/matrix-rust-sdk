use napi_derive::napi;
use ruma::{
    api::client::r0::sync::sync_events::DeviceLists as RumaDeviceLists, assign, identifiers::UserId,
};
use serde::{Deserialize, Serialize};

#[napi(object)]
#[derive(Serialize, Deserialize)]
pub struct DeviceLists {
    pub changed: Vec<String>,
    pub left: Vec<String>,
}

impl From<DeviceLists> for RumaDeviceLists {
    fn from(d: DeviceLists) -> Self {
        assign!(RumaDeviceLists::new(), {
            changed: d
                .changed
                .into_iter()
                .filter_map(|u| Box::<UserId>::try_from(u).ok())
                .collect(),
            left: d
                .left
                .into_iter()
                .filter_map(|u| Box::<UserId>::try_from(u).ok())
                .collect(),
        })
    }
}

#[napi(object)]
#[derive(Serialize, Deserialize)]
/// An event that was successfully decrypted.
pub struct DecryptedEvent {
    /// The decrypted version of the event.
    pub clear_event: String,
    /// The claimed curve25519 key of the sender.
    pub sender_curve25519_key: String,
    /// The claimed ed25519 key of the sender.
    pub claimed_ed25519_key: Option<String>,
    /// The curve25519 chain of the senders that forwarded the Megolm decryption
    /// key to us. Is empty if the key came directly from the sender of the
    /// event.
    pub forwarding_curve25519_chain: Vec<String>,
}
