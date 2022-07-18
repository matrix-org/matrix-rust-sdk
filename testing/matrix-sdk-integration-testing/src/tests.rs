
use std::option_env;
use matrix_sdk::{
    store::make_store_config, Client, ClientBuilder,
    ruma::{
        api::client::{
            account::register::v3::Request as RegistrationRequest, room::Visibility, uiaa,
        },
    }
};
use anyhow::Result;
use tempfile::{tempdir, TempDir};
use assign::assign;
use lazy_static::lazy_static;

use std::collections::HashMap;
use std::sync::Mutex;

lazy_static! {
    static ref USERS: Mutex<HashMap<String, (Client, TempDir)>> = {
        Mutex::new(HashMap::new())
    };    
}

/// read the test configuration from the environment
pub fn test_server_conf() -> (String, String) {
    (
        option_env!("HOMSERVER_URL").unwrap_or("http://localhost:8228").to_owned(),
        option_env!("HOMSERVER_DOMAIN").unwrap_or("matrix-sdk.rs").to_owned(),
    )
}


pub async fn get_client_for_user(username: String) -> Result<Client> {
    let mut users = USERS.lock().expect("Static doesn't fail");
    if let Some((client, _)) = users.get(&username) {
        return Ok(client.clone())
    }
    
    let (homeserver_url, _domain_name) = test_server_conf();

    let tmp_dir = tempdir()?;

    let client = Client::builder()
        .user_agent("matrix-sdk-integation-tests".to_owned())
        .store_config(make_store_config(tmp_dir.path(), None)?)
        .homeserver_url(homeserver_url)
        .build()
        .await?;
    // safe to assume we have not registered this user yet, but ignore if we did

    if let Err(resp) = client.register(RegistrationRequest::new()).await {
        // FIXME: do actually check the registration types...
        if let Some(_response) = resp.uiaa_response() {
            let request = assign!(RegistrationRequest::new(), {
                username: Some(username.as_ref()),
                password: Some(username.as_ref()),

                auth: Some(uiaa::AuthData::Dummy(uiaa::Dummy::new())),
            });
            // we don't care if this failed, then we just try to login anyways
            let _ = client.register(request).await;
        }
    }
    client.login_username(&username, &username).send().await?;
    users.insert(username, (client.clone(), tmp_dir)); // keeping temp dir around so it doesn't get destroyed yet

    Ok(client)

}

mod invitations;