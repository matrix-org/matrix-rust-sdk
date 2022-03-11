use std::{
    future,
    sync::{Arc, Mutex},
};

use matrix_sdk::{
    config::RequestConfig,
    ruma::{api::appservice::Registration, events::room::member::SyncRoomMemberEvent},
    Client,
};
use matrix_sdk_appservice::*;
use matrix_sdk_test::{appservice::TransactionBuilder, async_test, EventsJson};
use ruma::room_id;
use serde_json::json;
use warp::{Filter, Reply};

fn registration_string() -> String {
    include_str!("../tests/registration.yaml").to_owned()
}

async fn appservice(registration: Option<Registration>) -> Result<AppService> {
    // env::set_var(
    //     "RUST_LOG",
    //     "mockito=debug,matrix_sdk=debug,ruma=debug,warp=debug",
    // );
    let _ = tracing_subscriber::fmt::try_init();

    let registration = match registration {
        Some(registration) => registration.into(),
        None => AppServiceRegistration::try_from_yaml_str(registration_string()).unwrap(),
    };

    let homeserver_url = mockito::server_url();
    let server_name = "localhost";

    let client_builder = Client::builder()
        .request_config(RequestConfig::default().disable_retry())
        .check_supported_versions(false);

    AppService::with_client_builder(
        homeserver_url.as_ref(),
        server_name,
        registration,
        client_builder,
    )
    .await
}

#[async_test]
async fn test_register_virtual_user() -> Result<()> {
    let appservice = appservice(None).await?;

    let localpart = "someone";
    let _mock = mockito::mock("POST", "/_matrix/client/r0/register")
        .match_query(mockito::Matcher::Missing)
        .match_header(
            "authorization",
            mockito::Matcher::Exact(format!("Bearer {}", appservice.registration().as_token)),
        )
        .match_body(mockito::Matcher::Json(json!({
            "username": localpart.to_owned(),
            "type": "m.login.application_service"
        })))
        .with_body(format!(
            r#"{{
            "access_token": "abc123",
            "device_id": "GHTYAJCE",
            "user_id": "@{localpart}:localhost"
        }}"#,
            localpart = localpart
        ))
        .create();

    appservice.register_virtual_user(localpart).await?;

    Ok(())
}

#[async_test]
async fn test_put_transaction() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    let appservice = appservice(None).await?;

    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_get_user() -> Result<()> {
    let appservice = appservice(None).await?;
    appservice.register_user_query(Box::new(|_, _| Box::pin(async move { true }))).await;

    let uri = "/_matrix/app/v1/users/%40_botty_1%3Adev.famedly.local?access_token=hs_token";

    let status = warp::test::request()
        .method("GET")
        .path(uri)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_get_room() -> Result<()> {
    let appservice = appservice(None).await?;
    appservice.register_room_query(Box::new(|_, _| Box::pin(async move { true }))).await;

    let uri = "/_matrix/app/v1/rooms/%23magicforest%3Aexample.com?access_token=hs_token";

    let status = warp::test::request()
        .method("GET")
        .path(uri)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_invalid_access_token() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1?access_token=invalid_token";

    let mut transaction_builder = TransactionBuilder::new();
    let transaction =
        transaction_builder.add_room_event(EventsJson::Member).build_json_transaction();

    let appservice = appservice(None).await?;

    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    assert_eq!(status, 401);

    Ok(())
}

#[async_test]
async fn test_no_access_token() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    let appservice = appservice(None).await?;

    {
        let status = warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 401);
    }

    Ok(())
}

#[async_test]
async fn test_event_handler() -> Result<()> {
    let appservice = appservice(None).await?;

    #[allow(clippy::mutex_atomic)]
    let on_state_member = Arc::new(Mutex::new(false));
    appservice
        .register_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: SyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        })
        .await?;

    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap();

    let on_room_member_called = *on_state_member.lock().unwrap();
    assert!(on_room_member_called);

    Ok(())
}

#[async_test]
async fn test_unrelated_path() -> Result<()> {
    let appservice = appservice(None).await?;

    let status = {
        let consumer_filter = warp::any()
            .and(appservice.warp_filter())
            .or(warp::get().and(warp::path("unrelated").map(warp::reply)));

        let response = warp::test::request()
            .method("GET")
            .path("/unrelated")
            .filter(&consumer_filter)
            .await?
            .into_response();

        response.status()
    };

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_appservice_on_sub_path() -> Result<()> {
    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    let uri_1 = "/sub_path/_matrix/app/v1/transactions/1?access_token=hs_token";
    let uri_2 = "/sub_path/_matrix/app/v1/transactions/2?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction_1 = transaction_builder.build_json_transaction();

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::MemberNameChange);
    let transaction_2 = transaction_builder.build_json_transaction();

    let appservice = appservice(None).await?;

    {
        warp::test::request()
            .method("PUT")
            .path(uri_1)
            .json(&transaction_1)
            .filter(&warp::path("sub_path").and(appservice.warp_filter()))
            .await?;

        warp::test::request()
            .method("PUT")
            .path(uri_2)
            .json(&transaction_2)
            .filter(&warp::path("sub_path").and(appservice.warp_filter()))
            .await?;
    };

    let members = appservice
        .get_cached_client(None)?
        .get_room(room_id)
        .expect("Expected room to be available")
        .members_no_sync()
        .await?;

    assert_eq!(members[0].display_name().unwrap(), "changed");

    Ok(())
}

mod registration {
    use super::*;

    #[test]
    fn test_registration() -> Result<()> {
        let registration: Registration = serde_yaml::from_str(&registration_string())?;
        let registration: AppServiceRegistration = registration.into();

        assert_eq!(registration.id, "appservice");

        Ok(())
    }

    #[test]
    fn test_registration_from_yaml_file() -> Result<()> {
        let registration = AppServiceRegistration::try_from_yaml_file("./tests/registration.yaml")?;

        assert_eq!(registration.id, "appservice");

        Ok(())
    }

    #[test]
    fn test_registration_from_yaml_str() -> Result<()> {
        let registration = AppServiceRegistration::try_from_yaml_str(registration_string())?;

        assert_eq!(registration.id, "appservice");

        Ok(())
    }
}
