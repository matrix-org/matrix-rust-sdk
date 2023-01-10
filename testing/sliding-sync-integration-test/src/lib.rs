use matrix_sdk::{Client, SlidingSyncBuilder};
use matrix_sdk_integration_testing::helpers::get_client_for_user;

#[allow(dead_code)]
async fn setup(name: String) -> anyhow::Result<(Client, SlidingSyncBuilder)> {
    let sliding_sync_proxy_url =
        option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();
    let client = get_client_for_user(name, false).await?;
    let sliding_sync_builder = client
        .sliding_sync()
        .await
        .homeserver(sliding_sync_proxy_url.parse()?)
        .with_common_extensions();
    Ok((client, sliding_sync_builder))
}

#[cfg(test)]
mod tests {
    use futures::{pin_mut, stream::StreamExt};

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn it_works_smoke_test() -> anyhow::Result<()> {
        let (_client, sync_proxy_builder) = setup("odo".to_owned()).await?;
        let sync_proxy = sync_proxy_builder.add_fullsync_view().build().await?;
        let stream = sync_proxy.stream().await?;
        pin_mut!(stream);
        let Some(room_summary ) = stream.next().await else {
            anyhow::bail!("No room summary found, loop ended unsuccessfully");
        };
        let summary = room_summary?;
        assert_eq!(summary.rooms.len(), 0);
        Ok(())
    }
}
