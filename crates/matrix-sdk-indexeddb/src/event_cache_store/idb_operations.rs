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
    // Insert new value
    let id = format!("{}-{}", hashed_room_id, new);
    let chunk = Chunk { id, previous, new, next };
    let value = JsValue::from_serde(&chunk).unwrap();
    store.add_val(&value)?;

    // Update previous if there
    if let Some(previous) = previous {
        let previous_id = format!("{}-{}", hashed_room_id, previous);
        let previous_chunk: Chunk = store.get_val(&previous_id)?.unwrap();
        let updated_previous_chunk = Chunk {
            id: previous_chunk.id,
            previous: previous_chunk.previous,
            new: previous_chunk.new,
            next: Some(new),
        };
        let updated_previous_value = JsValue::from_serde(&updated_previous_chunk).unwrap();
        store.put_val(&updated_previous_value)?;
    }

    // update next if there
    if let Some(next) = next {
        let next_id = format!("{}-{}", hashed_room_id, next);
        let next_chunk: Chunk = store.get_val(&next_id)?.unwrap();
        let updated_next_chunk = Chunk {
            id: next_chunk.id,
            previous: Some(new),
            new: next_chunk.new,
            next: next_chunk.next,
        };
        let updated_next_value = JsValue::from_serde(&updated_next_chunk).unwrap();
        store.put_val(&updated_next_value)?;
    }

    Ok(())
}
