//! `GET /_matrix/client/*/sync`.

use napi_derive::*;

use crate::identifiers;

/// Information on E2E device updates.
#[napi]
pub struct DeviceLists {
    pub(crate) inner: ruma::api::client::sync::sync_events::v3::DeviceLists,
}

#[napi]
impl DeviceLists {
    /// Create an empty `DeviceLists`.
    #[napi(constructor)]
    pub fn new(
        changed: Option<Vec<&identifiers::UserId>>,
        left: Option<Vec<&identifiers::UserId>>,
    ) -> Self {
        let mut inner = ruma::api::client::sync::sync_events::v3::DeviceLists::default();

        inner.changed = changed.into_iter().flatten().map(|user| user.inner.clone()).collect();
        inner.left = left.into_iter().flatten().map(|user| user.inner.clone()).collect();

        Self { inner }
    }

    /// Returns true if there are no device list updates.
    #[napi]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// List of users who have updated their device identity keys or
    /// who now share an encrypted room with the client since the
    /// previous sync.
    #[napi(getter)]
    pub fn changed(&self) -> Vec<identifiers::UserId> {
        self.inner.changed.iter().map(|user| identifiers::UserId::from(user.to_owned())).collect()
    }

    /// List of users who no longer share encrypted rooms since the
    /// previous sync response.
    #[napi(getter)]
    pub fn left(&self) -> Vec<identifiers::UserId> {
        self.inner.left.iter().map(|user| identifiers::UserId::from(user.to_owned())).collect()
    }
}
