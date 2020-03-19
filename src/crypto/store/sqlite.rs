// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::sync::Arc;
use url::Url;

use async_trait::async_trait;
use olm_rs::PicklingMode;
use sqlx::{query, query_as, sqlite::SqliteQueryAs, Connect, Executor, SqliteConnection};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use super::{Account, CryptoStore, Result};

pub struct SqliteStore {
    user_id: Arc<String>,
    device_id: Arc<String>,
    path: PathBuf,
    connection: Arc<Mutex<SqliteConnection>>,
    pickle_passphrase: Option<Zeroizing<String>>,
}

static DATABASE_NAME: &str = "matrix-sdk-crypto.db";

impl SqliteStore {
    pub async fn open<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, None).await
    }

    pub async fn open_with_passphrase<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
        passphrase: String,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, Some(Zeroizing::new(passphrase))).await
    }

    fn path_to_url(path: &Path) -> Result<Url> {
        // TODO this returns an empty error if the path isn't absolute.
        let url = Url::from_directory_path(path).expect("Invalid path");
        Ok(url.join(DATABASE_NAME)?)
    }

    async fn open_helper<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
        passphrase: Option<Zeroizing<String>>,
    ) -> Result<SqliteStore> {
        let url = SqliteStore::path_to_url(path.as_ref())?;

        let connection = SqliteConnection::connect(url.as_ref()).await.unwrap();
        let store = SqliteStore {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.to_owned()),
            path: path.as_ref().to_owned(),
            connection: Arc::new(Mutex::new(connection)),
            pickle_passphrase: passphrase,
        };
        store.create_tables().await?;
        Ok(store)
    }

    async fn create_tables(&self) -> Result<()> {
        let mut connection = self.connection.lock().await;
        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS account (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                "shared" INTEGER NOT NULL,
                UNIQUE(user_id,device_id)
            );
        "#,
            )
            .await?;

        Ok(())
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        match &self.pickle_passphrase {
            Some(p) => PicklingMode::Encrypted {
                key: p.as_bytes().to_vec(),
            },
            None => PicklingMode::Unencrypted,
        }
    }
}

#[async_trait]
impl CryptoStore for SqliteStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
        let mut connection = self.connection.lock().await;

        let row: Option<(String, bool)> = query_as(
            "SELECT pickle, shared FROM account
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .fetch_optional(&mut *connection)
        .await?;

        let result = match row {
            Some((pickle, shared)) => Some(Account::from_pickle(
                pickle,
                self.get_pickle_mode(),
                shared,
            )?),
            None => None,
        };

        Ok(result)
    }

    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()> {
        let acc = account.lock().await;
        let pickle = acc.pickle(self.get_pickle_mode());
        let mut connection = self.connection.lock().await;

        query(
            "INSERT OR IGNORE INTO account (
                user_id, device_id, pickle, shared
             ) VALUES (?, ?, ?, ?)",
        )
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .bind(&pickle)
        .bind(acc.shared)
        .execute(&mut *connection)
        .await?;

        query(
            "UPDATE account
               SET pickle = ?,
                   shared = ?
               WHERE user_id = ? and
                    device_id = ?",
        )
        .bind(pickle)
        .bind(acc.shared)
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        write!(
            fmt,
            "SqliteStore {{ user_id: {}, device_id: {}, path: {:?} }}",
            self.user_id, self.device_id, self.path
        )
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::{Account, CryptoStore, SqliteStore};

    static USER_ID: &str = "@example:localhost";
    static DEVICE_ID: &str = "DEVICEID";

    async fn get_store() -> SqliteStore {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        SqliteStore::open(USER_ID, DEVICE_ID, tmpdir_path)
            .await
            .expect("Can't create store")
    }

    fn get_account() -> Arc<Mutex<Account>> {
        let account = Account::new();
        Arc::new(Mutex::new(account))
    }

    #[tokio::test]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SqliteStore::open("@example:localhost", "DEVICEID", tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test]
    async fn save_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test]
    async fn load_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let acc = account.lock().await;
        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(*acc, loaded_account);
    }

    #[tokio::test]
    async fn save_and_share_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        account.lock().await.shared = true;

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();
        let acc = account.lock().await;

        assert_eq!(*acc, loaded_account);
    }
}
