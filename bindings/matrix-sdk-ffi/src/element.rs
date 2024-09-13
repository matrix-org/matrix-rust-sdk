use serde::Deserialize;

use crate::ClientError;

/// Well-known settings specific to ElementCall
#[derive(Deserialize, uniffi::Record)]
pub struct ElementCallWellKnown {
    widget_url: String,
}

/// Element specific well-known settings
#[derive(Deserialize, uniffi::Record)]
pub struct ElementWellKnown {
    call: Option<ElementCallWellKnown>,
    registration_helper_url: Option<String>,
}

/// Helper function to parse a string into a ElementWellKnown struct
#[uniffi::export]
pub fn make_element_well_known(string: String) -> Result<ElementWellKnown, ClientError> {
    serde_json::from_str(&string).map_err(ClientError::new)
}
