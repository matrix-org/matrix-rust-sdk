use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::idb_object_store::IdbObjectStore;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

#[derive(Serialize, Deserialize)]
struct Chunk {
    id: String,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
}

pub async fn insert_chunk(
    store: IdbObjectStore<'_>,
    hashed_room_id: &String,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
) -> Result<(), web_sys::DomException> {
    let id = format!("{}-{}", hashed_room_id, new);
    let chunk = Chunk { id, previous, new, next };
    let value = JsValue::from_serde(&chunk).unwrap();
    store.add_val(&value)?;
    Ok(())
}
