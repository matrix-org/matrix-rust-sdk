use crate::helpers::TestClientBuilder;
use anyhow::Result;

#[tokio::test(flavor = "multi_thread")]
async fn register_login_is_one_device() -> Result<()> {
    // registers a fresh client and makes sure we are logged in
    let client = TestClientBuilder::new("alice").randomize_username().use_sqlite().build().await?;
    // should leave us with exactly one device logged in, right?!?
    let devices = client.devices().await?.devices;
    assert_eq!(devices.len(), 1);
    Ok(())
}
