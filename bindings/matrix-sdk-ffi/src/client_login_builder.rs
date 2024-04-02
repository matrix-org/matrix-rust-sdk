use std::sync::{Arc, RwLock};

use matrix_sdk::matrix_auth::LoginBuilder;

use crate::ClientError;

/// Builder type used to configure optional settings for logging in to a matrix
/// server
#[derive(Clone, uniffi::Object)]
pub struct ClientLoginBuilder {
    /// Because [LoginBuilder] doesn't implement Clone the options where to
    /// either make this a shadow version of LoginBuilder (with all the data
    /// stored inline) or to store the builder in a way that makes it
    /// consumable. By using [RwLock] with an [Option] inside the latter was
    /// achieved with an error condition if [send] is called twice.
    inner: Arc<RwLock<Option<LoginBuilder>>>,
}

impl ClientLoginBuilder {
    pub(crate) fn new(builder: LoginBuilder) -> Self {
        Self { inner: Arc::new(RwLock::new(Some(builder))) }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl ClientLoginBuilder {
    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the homeserver will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    pub fn device_id(self: Arc<Self>, value: String) -> Arc<Self> {
        {
            let mut inner = self.inner.write().unwrap();
            if let Some(builder) = inner.take() {
                *inner = Some(builder.device_id(&value));
            }
        }
        self
    }

    /// Set the initial device display name.
    ///
    /// The device display name is the public name that will be associated with
    /// the device ID. Only necessary the first time you log in with this device
    /// ID. It can be changed later.
    pub fn initial_device_display_name(self: Arc<Self>, value: String) -> Arc<Self> {
        {
            let mut inner = self.inner.write().unwrap();
            if let Some(builder) = inner.take() {
                *inner = Some(builder.initial_device_display_name(&value));
            }
        }
        self
    }

    /// Send the login request.
    pub async fn send(self: Arc<Self>) -> Result<(), ClientError> {
        let builder = self.inner.write().unwrap().take().ok_or_else(|| ClientError::Generic {
            msg: "ClientLoginBuilder is already consumed".to_owned(),
        })?;
        builder.send().await?;
        Ok(())
    }
}
