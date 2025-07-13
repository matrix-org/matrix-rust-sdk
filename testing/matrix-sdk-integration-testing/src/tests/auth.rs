use anyhow::Result;

use crate::helpers::TestClientBuilder;

#[tokio::test(flavor = "multi_thread")]
async fn register_login_is_one_device() -> Result<()> {
    // registers a fresh client and makes sure we are logged in
    let client = TestClientBuilder::new("alice").use_sqlite().build().await?;
    let devices = client.devices().await?.devices;
    assert_eq!(devices.len(), 1);
    Ok(())
}
