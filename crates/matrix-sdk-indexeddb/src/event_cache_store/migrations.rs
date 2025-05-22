// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// limitations under the License

use indexed_db_futures::{
    idb_object_store::IdbObjectStoreParameters,
    request::{IdbOpenDbRequestLike, OpenDbRequest},
    IdbDatabase, IdbKeyPath, IdbVersionChangeEvent,
};
use wasm_bindgen::JsValue;
use web_sys::DomException;

use crate::event_cache_store::keys;

const CURRENT_DB_VERSION: u32 = 2;

pub async fn open_and_upgrade_db(name: &str) -> Result<IdbDatabase, DomException> {
    let mut db = IdbDatabase::open(name)?.await?;
    let old_version = db.version() as u32;
    if old_version == 1 {
        db = setup_db(db, CURRENT_DB_VERSION).await?;
    }
    Ok(db)
}

async fn setup_db(db: IdbDatabase, version: u32) -> Result<IdbDatabase, DomException> {
    let name = db.name();
    db.close();

    let db = {
        let mut request: OpenDbRequest = IdbDatabase::open_u32(&name, version)?;
        request.set_on_upgrade_needed(Some(
            move |events: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                events.db().create_object_store(keys::CORE)?;

                let mut params = IdbObjectStoreParameters::new();
                params.key_path(Some(&IdbKeyPath::from("id")));
                events.db().create_object_store_with_params(keys::LINKED_CHUNKS, &params)?;
                events
                    .db()
                    .create_object_store_with_params(keys::EVENTS, &params)?
                    .create_index(keys::EVENT_POSITIONS, &IdbKeyPath::from("position"))?;
                events.db().create_object_store_with_params(keys::GAPS, &params)?;
                Ok(())
            },
        ));
        request.await?
    };

    Ok(db)
}
