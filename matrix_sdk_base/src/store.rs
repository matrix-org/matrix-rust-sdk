use serde_json;

use sled::{transaction::TransactionalTree, Config, Db, Tree};

#[derive(Debug, Clone)]
pub struct Store {
    inner: Db,
    session_tree: Tree,
}

use crate::Session;

pub struct TransactionalStore<'a> {
    inner: &'a TransactionalTree,
}

impl<'a> std::fmt::Debug for TransactionalStore<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionalStore").finish()
    }
}

impl<'a> TransactionalStore<'a> {
    pub fn store_session(&self, session: &Session) {
        self.inner
            .insert("session", serde_json::to_vec(session).unwrap())
            .unwrap();
    }
}

impl Store {
    pub fn open() -> Self {
        let db = Config::new().temporary(true).open().unwrap();
        let session_tree = db.open_tree("session").unwrap();

        Self {
            inner: db,
            session_tree,
        }
    }

    pub fn get_session(&self) -> Option<Session> {
        self.session_tree
            .get("session")
            .unwrap()
            .map(|s| serde_json::from_slice(&s).unwrap())
    }

    pub async fn transaction<F, R>(&self, callback: F) -> R
    where
        F: Fn(&TransactionalStore) -> R,
    {
        let ret = self
            .session_tree
            .transaction::<_, _, ()>(|t| {
                let transaction = TransactionalStore { inner: t };
                Ok(callback(&transaction))
            })
            .unwrap();

        self.inner.flush_async().await.unwrap();

        ret
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_common::identifiers::{user_id, DeviceIdBox, UserId};
    use matrix_sdk_test::async_test;

    use super::Store;
    use crate::Session;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn device_id() -> DeviceIdBox {
        "DEVICEID".into()
    }

    #[async_test]
    async fn test_session_saving() {
        let session = Session {
            user_id: user_id(),
            device_id: device_id().into(),
            access_token: "TEST_TOKEN".to_owned(),
        };

        let store = Store::open();

        store
            .transaction(|t| {
                t.store_session(&session);
                ()
            })
            .await;
        let stored_session = store.get_session().unwrap();

        assert_eq!(session, stored_session);
    }
}
