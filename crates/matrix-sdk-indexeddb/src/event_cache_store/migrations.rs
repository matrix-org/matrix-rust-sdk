use indexed_db_futures::IdbDatabase;

use super::{indexeddb_serializer::IndexeddbSerializer, IndexeddbEventCacheStoreError};

pub async fn open_and_upgrade_db(
    name: &str,
    _serializer: &IndexeddbSerializer,
) -> Result<IdbDatabase, IndexeddbEventCacheStoreError> {
    Ok(IdbDatabase::open(name)?.await?)
}
