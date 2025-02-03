use super::{indexeddb_serializer::IndexeddbSerializer, Result};
use indexed_db_futures::IdbDatabase;

pub async fn open_and_upgrade_db(
    name: &str,
    _serializer: &IndexeddbSerializer,
) -> Result<IdbDatabase> {
    let db = IdbDatabase::open(name)?.await?;
    Ok(db)
}
