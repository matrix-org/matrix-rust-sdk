#![cfg(test)]

use crate::helpers::get_client_for_user;
use anyhow::Context;
use futures_util::{pin_mut, stream::StreamExt};
use matrix_sdk::{Client, SlidingSyncBuilder, SlidingSyncList, SlidingSyncMode};

mod notification_client;

async fn setup(
    name: String,
    use_sqlite_store: bool,
) -> anyhow::Result<(Client, SlidingSyncBuilder)> {
    let sliding_sync_proxy_url =
        option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();
    let client = get_client_for_user(name, use_sqlite_store).await?;
    let sliding_sync_builder = client
        .sliding_sync("test-slidingsync")?
        .sliding_sync_proxy(sliding_sync_proxy_url.parse()?);
    Ok((client, sliding_sync_builder))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_works_smoke_test() -> anyhow::Result<()> {
    let (_client, sync_builder) = setup("odo".to_owned(), false).await?;
    let sync_proxy = sync_builder
        .add_list(
            SlidingSyncList::builder("foo")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))
                .timeline_limit(0),
        )
        .build()
        .await?;
    let stream = sync_proxy.sync();
    pin_mut!(stream);
    let room_summary =
        stream.next().await.context("No room summary found, loop ended unsuccessfully")?;
    let summary = room_summary?;
    assert_eq!(summary.rooms.len(), 0);
    Ok(())
}
