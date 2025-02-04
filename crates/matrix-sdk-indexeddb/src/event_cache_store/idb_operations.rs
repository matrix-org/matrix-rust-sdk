use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::{idb_object_store::IdbObjectStore, IdbQuerySource};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

#[derive(Serialize, Deserialize)]
struct Chunk {
    id: String,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
    type_str: String,
}

pub async fn insert_chunk(
    store: &IdbObjectStore<'_>,
    hashed_room_id: &String,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
    type_str: &str,
) -> Result<(), web_sys::DomException> {
    // Insert new value
    let id = format!("{}-{}", hashed_room_id, new);
    let chunk = Chunk { id, previous, new, next, type_str: type_str.to_owned() };
    let value = JsValue::from_serde(&chunk).unwrap();
    store.add_val(&value)?;

    // Update previous if there
    if let Some(previous) = previous {
        let previous_id = format!("{}-{}", hashed_room_id, previous);
        // TODO unsafe unwrap()?
        let previous_chunk_js_value = store.get_owned(&previous_id)?.await?.unwrap();
        let previous_chunk: Chunk = previous_chunk_js_value.into_serde().unwrap();
        let updated_previous_chunk = Chunk {
            id: previous_chunk.id,
            previous: previous_chunk.previous,
            new: previous_chunk.new,
            next: Some(new),
            type_str: previous_chunk.type_str,
        };
        let updated_previous_value = JsValue::from_serde(&updated_previous_chunk).unwrap();
        store.put_val(&updated_previous_value)?;
    }

    // update next if there
    if let Some(next) = next {
        let next_id = format!("{}-{}", hashed_room_id, next);
        // TODO unsafe unwrap()?
        let next_chunk_js_value = store.get_owned(&next_id)?.await?.unwrap();
        let next_chunk: Chunk = next_chunk_js_value.into_serde().unwrap();
        let updated_next_chunk = Chunk {
            id: next_chunk.id,
            previous: Some(new),
            new: next_chunk.new,
            next: next_chunk.next,
            type_str: next_chunk.type_str,
        };
        let updated_next_value = JsValue::from_serde(&updated_next_chunk).unwrap();
        store.put_val(&updated_next_value)?;
    }

    Ok(())
}
