use super::{indexeddb_serializer::IndexeddbSerializer, keys, Result};
use indexed_db_futures::{
    idb_object_store::IdbObjectStoreParameters,
    request::{IdbOpenDbRequestLike, OpenDbRequest},
    IdbDatabase, IdbKeyPath, IdbVersionChangeEvent,
};
use wasm_bindgen::JsValue;

const CURRENT_DB_VERSION: u32 = 1;

pub async fn open_and_upgrade_db(
    name: &str,
    _serializer: &IndexeddbSerializer,
) -> Result<IdbDatabase> {
    let mut db = IdbDatabase::open(name)?.await?;

    let old_version = db.version() as u32;

    if old_version == 0 {
        // TODO some temporary code just to get going
        // Take a look at the state_store migrations
        // https://github.com/ospfranco/matrix-rust-sdk/blob/e49bda6f821d1b117c623dc9682e22337be16149/crates/matrix-sdk-indexeddb/src/state_store/migrations.rs
        db = setup_db(db, CURRENT_DB_VERSION).await?;
    }

    Ok(db)
}

async fn setup_db(db: IdbDatabase, version: u32) -> Result<IdbDatabase> {
    let name = db.name();
    db.close();

    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(&name, version)?;
    db_req.set_on_upgrade_needed(Some(
        move |events: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            let mut params = IdbObjectStoreParameters::new();
            params.key_path(Some(&IdbKeyPath::from("id")));
            events.db().create_object_store_with_params(keys::LINKED_CHUNKS, &params);
            Ok(())
        },
    ));

    let db = db_req.await?;

    Ok(db)
}
