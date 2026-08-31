Use `Readonly` rather than `Readwrite` transactions in
`EventCacheStore::{load_all_chunks, load_all_chunks_metadata}`
as these functions only read data from the database.
