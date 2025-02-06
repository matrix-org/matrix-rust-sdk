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

pub fn insert_gap(
    store: &IdbObjectStore<'_>,
    hashed_room_id: &String,
    new: u64,
    prev_token: &JsValue,
) -> Result<(), web_sys::DomException> {
    let id = format!("{}-{}", hashed_room_id, new);
    let id = JsValue::from_str(&id);
    store.add_key_val(&id, prev_token)?;

    Ok(())
}

pub async fn remove_chunk(
    store: &IdbObjectStore<'_>,
    hashed_room_id: &String,
    id: u64,
) -> Result<(), web_sys::DomException> {
    let id = format!("{}-{}", hashed_room_id, id);

    // Get current value, so we can later update prev and next
    let chunk_to_delete_js_value = store.get_owned(id)?.await?.unwrap();
    let chunk_to_delete: Chunk = chunk_to_delete_js_value.into_serde().unwrap();

    // Get previous value
    if let Some(previous) = chunk_to_delete.previous {
        let previous_id = format!("{}-{}", hashed_room_id, previous);
        let previous_chunk_js_value = store.get_owned(previous_id)?.await?.unwrap();
        let mut previous_chunk: Chunk = previous_chunk_js_value.into_serde().unwrap();

        previous_chunk.next = chunk_to_delete.next;

        // save modified chunk
        let updated_previous_value = JsValue::from_serde(&previous_chunk).unwrap();
        store.put_val(&updated_previous_value)?;
    }

    // Get next value if there and update it's previous
    if let Some(next) = chunk_to_delete.next {
        let next_id = format!("{}-{}", hashed_room_id, next);
        let next_chunk_js_value = store.get_owned(next_id)?.await?.unwrap();
        let mut next_chunk: Chunk = next_chunk_js_value.into_serde().unwrap();

        next_chunk.previous = chunk_to_delete.previous;

        // save modified chunk
        let updated_next_value = JsValue::from_serde(&next_chunk).unwrap();
        store.put_val(&updated_next_value)?;
    }

    store.delete_owned(chunk_to_delete.id)?;

    // TODO on the SQLite version, there is a cascading delete that deletes related entities
    // we will have to implement those manually here

    Ok(())
}
