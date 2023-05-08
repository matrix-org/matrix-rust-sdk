// Copyright 2022 The Matrix.org Foundation C.I.C.
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
#![cfg_attr(
    not(any(feature = "state-store", feature = "crypto-store")),
    allow(dead_code, unused_imports)
)]

use deadpool_sqlite::Object as SqliteConn;
use matrix_sdk_store_encryption::StoreCipher;

#[cfg(feature = "crypto-store")]
mod crypto_store;
mod error;
#[cfg(feature = "state-store")]
mod state_store;
mod utils;

#[cfg(feature = "crypto-store")]
pub use self::crypto_store::SqliteCryptoStore;
pub use self::error::OpenStoreError;
#[cfg(feature = "state-store")]
pub use self::state_store::SqliteStateStore;
use self::utils::SqliteObjectStoreExt;

async fn get_or_create_store_cipher(
    passphrase: &str,
    conn: &SqliteConn,
) -> Result<StoreCipher, OpenStoreError> {
    let encrypted_cipher = conn.get_kv("cipher").await.map_err(OpenStoreError::LoadCipher)?;

    let cipher = if let Some(encrypted) = encrypted_cipher {
        StoreCipher::import(passphrase, &encrypted)?
    } else {
        let cipher = StoreCipher::new()?;
        #[cfg(not(test))]
        let export = cipher.export(passphrase);
        #[cfg(test)]
        let export = cipher._insecure_export_fast_for_testing(passphrase);
        conn.set_kv("cipher", export?).await.map_err(OpenStoreError::SaveCipher)?;
        cipher
    };

    Ok(cipher)
}

#[cfg(test)]
#[ctor::ctor]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}
