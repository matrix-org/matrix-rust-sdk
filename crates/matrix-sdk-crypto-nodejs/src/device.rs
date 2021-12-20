use matrix_sdk_crypto::{Device as RSDevice};
use napi_derive::napi;
use serde_json::{Map, Value};
use serde::{Deserialize, Serialize};

#[napi(object)]
#[derive(Serialize, Deserialize)]
pub struct Device {
    pub user_id: String,
    pub device_id: String,
    pub keys: Map<String, Value>,
    pub algorithms: Vec<String>,
    pub display_name: Option<String>,
    pub is_blocked: bool,
    pub locally_trusted: bool,
    pub cross_signing_trusted: bool,
}

impl From<RSDevice> for Device {
    fn from(d: RSDevice) -> Self {
        Device {
            user_id: d.user_id().to_string(),
            device_id: d.device_id().to_string(),
            keys: d
                .keys()
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect::<Map<String, Value>>()
                .into(),
            algorithms: d.algorithms().iter().map(|a| a.to_string()).collect(),
            display_name: d.display_name().map(|d| d.to_owned()),
            is_blocked: d.is_blacklisted(),
            locally_trusted: d.is_locally_trusted(),
            cross_signing_trusted: d.is_cross_signing_trusted(),
        }
    }
}
