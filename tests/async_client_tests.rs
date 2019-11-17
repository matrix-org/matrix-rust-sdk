use matrix_nio::{AsyncClient, AsyncClientConfig};
use mockito::mock;
use std::str::FromStr;
use tokio::runtime::Runtime;
use url::Url;

#[test]
fn login() {
    let rt = Runtime::new().unwrap();

    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body_from_file("tests/data/login_response.json")
        .create();

    let mut client = AsyncClient::new(homeserver, None).unwrap();

    rt.block_on(client.login("example", "wordpass", None))
        .unwrap();

    assert!(client.logged_in(), "Clint should be logged in");
}
