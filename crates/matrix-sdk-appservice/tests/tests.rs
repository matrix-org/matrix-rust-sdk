use std::{
    future,
    sync::{Arc, Mutex},
};

use matrix_sdk::{
    config::RequestConfig,
    ruma::{api::appservice::Registration, events::room::member::OriginalSyncRoomMemberEvent},
    Client,
};
use matrix_sdk_appservice::*;
use matrix_sdk_test::{appservice::TransactionBuilder, async_test, EventsJson};
use ruma::{
    api::{appservice::event::push_events, MatrixVersion},
    events::AnyRoomEvent,
    room_id,
    serde::Raw,
};
use serde_json::json;
use warp::{Filter, Reply};
use wiremock::{
    matchers::{body_json, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

fn registration_string() -> String {
    include_str!("../tests/registration.yaml").to_owned()
}

async fn appservice(
    homeserver_url: Option<String>,
    registration: Option<Registration>,
) -> Result<AppService> {
    // env::set_var(
    //     "RUST_LOG",
    //     "wiremock=debug,matrix_sdk=debug,ruma=debug,warp=debug",
    // );
    let _ = tracing_subscriber::fmt::try_init();

    let registration = match registration {
        Some(registration) => registration.into(),
        None => AppServiceRegistration::try_from_yaml_str(registration_string()).unwrap(),
    };

    let homeserver_url = homeserver_url.unwrap_or_else(|| "http://localhost:1234".to_owned());
    let server_name = "localhost";

    let client_builder = Client::builder()
        .request_config(RequestConfig::default().disable_retry())
        .server_versions([MatrixVersion::V1_0]);

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
    let server = MockServer::start().await;
    let appservice = appservice(Some(server.uri()), None).await?;

    let localpart = "someone";
    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/register"))
        .and(header(
            "authorization",
            format!("Bearer {}", appservice.registration().as_token).as_str(),
        ))
        .and(body_json(json!({
            "username": localpart.to_owned(),
            "type": "m.login.application_service"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "abc123",
            "device_id": "GHTYAJCE",
            "user_id": format!("@{localpart}:localhost"),
        })))
        .mount(&server)
        .await;

    appservice.register_virtual_user(localpart).await?;

    Ok(())
}

#[async_test]
async fn test_put_transaction() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    let appservice = appservice(None, None).await?;

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
async fn test_put_transaction_with_repeating_txn_id() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    let appservice = appservice(None, None).await?;

    #[allow(clippy::mutex_atomic)]
    let on_state_member = Arc::new(Mutex::new(false));
    appservice
        .virtual_user(None)
        .await?
        .register_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        })
        .await;

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
    {
        let on_room_member_called = *on_state_member.lock().unwrap();
        assert!(on_room_member_called);
    }

    // Reset this to check that next time it doesnt get called
    {
        let mut on_room_member_called = on_state_member.lock().unwrap();
        *on_room_member_called = false;
    }

    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    // According to https://spec.matrix.org/v1.2/application-service-api/#pushing-events
    // This should noop and return 200.
    assert_eq!(status, 200);
    {
        let on_room_member_called = *on_state_member.lock().unwrap();
        // This time we should not have called the event handler.
        assert!(!on_room_member_called);
    }

    Ok(())
}

#[async_test]
async fn test_get_user() -> Result<()> {
    let appservice = appservice(None, None).await?;
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
    let appservice = appservice(None, None).await?;
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

    let appservice = appservice(None, None).await?;

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

    let appservice = appservice(None, None).await?;

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
    let appservice = appservice(None, None).await?;

    #[allow(clippy::mutex_atomic)]
    let on_state_member = Arc::new(Mutex::new(false));
    appservice
        .virtual_user(None)
        .await?
        .register_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        })
        .await;

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
    let appservice = appservice(None, None).await?;

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

    let appservice = appservice(None, None).await?;

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
        .virtual_user(None)
        .await?
        .get_room(room_id)
        .expect("Expected room to be available")
        .members_no_sync()
        .await?;

    assert_eq!(members[0].display_name().unwrap(), "changed");

    Ok(())
}

#[async_test]
async fn test_receive_transaction() -> Result<()> {
    tracing_subscriber::fmt().try_init().ok();
    let json = vec![
        Raw::new(&json!({
            "content": {
                "avatar_url": null,
                "displayname": "Appservice",
                "membership": "join"
            },
            "event_id": "$151800140479rdvjg:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "sender": "@_appservice:localhost",
            "state_key": "@_appservice:localhost",
            "type": "m.room.member",
            "room_id": "!coolplace:localhost",
            "unsigned": {
                "age": 2970366
            }
        }))?
        .cast::<AnyRoomEvent>(),
        Raw::new(&json!({
            "content": {
                "avatar_url": null,
                "displayname": "Appservice",
                "membership": "join"
            },
            "event_id": "$151800140491rfbja:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "sender": "@_appservice:localhost",
            "state_key": "@_appservice:localhost",
            "type": "m.room.member",
            "room_id": "!boringplace:localhost",
            "unsigned": {
                "age": 2970366
            }
        }))?
        .cast::<AnyRoomEvent>(),
        Raw::new(&json!({
            "content": {
                "avatar_url": null,
                "displayname": "Alice",
                "membership": "join"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "sender": "@_appservice_alice:localhost",
            "state_key": "@_appservice_alice:localhost",
            "type": "m.room.member",
            "room_id": "!coolplace:localhost",
            "unsigned": {
                "age": 2970366
            }
        }))?
        .cast::<AnyRoomEvent>(),
        Raw::new(&json!({
            "content": {
                "avatar_url": null,
                "displayname": "Bob",
                "membership": "invite"
            },
            "event_id": "$151800140594rfvjc:localhost",
            "membership": "invite",
            "origin_server_ts": 151800174,
            "sender": "@_appservice_bob:localhost",
            "state_key": "@_appservice_bob:localhost",
            "type": "m.room.member",
            "room_id": "!boringplace:localhost",
            "unsigned": {
                "age": 2970366
            }
        }))?
        .cast::<AnyRoomEvent>(),
    ];
    let appservice = appservice(None, None).await?;

    let alice = appservice.virtual_user(Some("_appservice_alice")).await?;
    let bob = appservice.virtual_user(Some("_appservice_bob")).await?;
    appservice
        .receive_transaction(push_events::v1::IncomingRequest::new("dontcare".into(), json))
        .await?;
    let coolplace = room_id!("!coolplace:localhost");
    let boringplace = room_id!("!boringplace:localhost");
    assert!(
        alice.get_joined_room(coolplace).is_some(),
        "Alice's membership in coolplace should be join"
    );
    assert!(
        bob.get_invited_room(boringplace).is_some(),
        "Bob's membership in boringplace should be invite"
    );
    assert!(alice.get_room(boringplace).is_none(), "Alice should not know about boringplace");
    assert!(bob.get_room(coolplace).is_none(), "Bob should not know about coolplace");
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
