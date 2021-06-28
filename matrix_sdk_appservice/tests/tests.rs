use std::{
    convert::TryFrom,
    sync::{Arc, Mutex},
};

#[cfg(feature = "actix")]
use actix_web::{test as actix_test, App as ActixApp, HttpResponse};
use matrix_sdk::{
    async_trait,
    room::Room,
    ruma::{
        api::appservice::Registration,
        events::{room::member::MemberEventContent, SyncStateEvent},
    },
    ClientConfig, EventHandler, RequestConfig,
};
use matrix_sdk_appservice::*;
use matrix_sdk_test::{appservice::TransactionBuilder, async_test, test_json, EventsJson};
use mockito::{mock, Matcher};
use ruma::{room_id, user_id, RoomIdOrAliasId};
use serde_json::json;
use urlencoding::encode;
#[cfg(feature = "warp")]
use warp::{Filter, Reply};

fn registration_string() -> String {
    include_str!("../tests/registration.yaml").to_owned()
}

async fn appservice(registration: Option<Registration>) -> Result<AppService> {
    // std::env::set_var(
    //     "RUST_LOG",
    //     vec![
    //         "matrix_sdk=trace",
    //         "matrix_sdk_appservice=trace",
    //         "mockito=debug",
    //         "ruma=debug",
    //         "actix_web=debug",
    //         "warp=debug",
    //     ]
    //     .join(","),
    // );
    let _ = tracing_subscriber::fmt::try_init();

    let registration = match registration {
        Some(registration) => registration.into(),
        None => AppServiceRegistration::try_from_yaml_str(registration_string()).unwrap(),
    };

    let homeserver_url = mockito::server_url();
    let server_name = "localhost";

    let client_config =
        ClientConfig::default().request_config(RequestConfig::default().disable_retry());

    Ok(AppService::new_with_config(
        homeserver_url.as_ref(),
        server_name,
        registration,
        client_config,
    )
    .await?)
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

    #[cfg(feature = "warp")]
    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    #[cfg(feature = "actix")]
    let status = {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::put().uri(uri).set_json(&transaction).to_request();

        actix_test::call_service(&app, req).await.status()
    };

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_get_user() -> Result<()> {
    let appservice = appservice(None).await?;

    let uri = "/_matrix/app/v1/users/%40_botty_1%3Adev.famedly.local?access_token=hs_token";

    #[cfg(feature = "warp")]
    let status = warp::test::request()
        .method("GET")
        .path(uri)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    #[cfg(feature = "actix")]
    let status = {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::get().uri(uri).to_request();

        actix_test::call_service(&app, req).await.status()
    };

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_get_room() -> Result<()> {
    let appservice = appservice(None).await?;

    let uri = "/_matrix/app/v1/rooms/%23magicforest%3Aexample.com?access_token=hs_token";

    #[cfg(feature = "warp")]
    let status = warp::test::request()
        .method("GET")
        .path(uri)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    #[cfg(feature = "actix")]
    let status = {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::get().uri(uri).to_request();

        actix_test::call_service(&app, req).await.status()
    };

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

    #[cfg(feature = "warp")]
    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    #[cfg(feature = "actix")]
    let status = {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::put().uri(uri).set_json(&transaction).to_request();

        actix_test::call_service(&app, req).await.status()
    };

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

    #[cfg(feature = "warp")]
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

    #[cfg(feature = "actix")]
    {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::put().uri(uri).set_json(&transaction).to_request();

        let resp = actix_test::call_service(&app, req).await;

        // TODO: this should actually return a 401 but is 500 because something in the
        // extractor fails
        assert_eq!(resp.status(), 500);
    }

    Ok(())
}

#[async_test]
async fn test_event_handler() -> Result<()> {
    let mut appservice = appservice(None).await?;

    #[derive(Clone)]
    struct Example {
        pub on_state_member: Arc<Mutex<bool>>,
    }

    impl Example {
        pub fn new() -> Self {
            #[allow(clippy::mutex_atomic)]
            Self { on_state_member: Arc::new(Mutex::new(false)) }
        }
    }

    #[async_trait]
    impl EventHandler for Example {
        async fn on_room_member(&self, _: Room, _: &SyncStateEvent<MemberEventContent>) {
            let on_state_member = self.on_state_member.clone();
            *on_state_member.lock().unwrap() = true;
        }
    }

    let example = Example::new();
    appservice.set_event_handler(Box::new(example.clone())).await?;

    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    #[cfg(feature = "warp")]
    warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap();

    #[cfg(feature = "actix")]
    {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::put().uri(uri).set_json(&transaction).to_request();

        actix_test::call_service(&app, req).await;
    };

    let on_room_member_called = *example.on_state_member.lock().unwrap();
    assert!(on_room_member_called);

    Ok(())
}

#[async_test]
async fn test_unrelated_path() -> Result<()> {
    let appservice = appservice(None).await?;

    #[cfg(feature = "warp")]
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

    #[cfg(feature = "actix")]
    let status = {
        let app = actix_test::init_service(
            ActixApp::new()
                .configure(appservice.actix_configure())
                .route("/unrelated", actix_web::web::get().to(HttpResponse::Ok)),
        )
        .await;

        let req = actix_test::TestRequest::get().uri("/unrelated").to_request();

        actix_test::call_service(&app, req).await.status()
    };

    assert_eq!(status, 200);

    Ok(())
}

#[async_test]
async fn test_transaction_join_room() -> Result<()> {
    let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

    let mut transaction_builder = TransactionBuilder::new();
    transaction_builder.add_room_event(EventsJson::Member);
    let transaction = transaction_builder.build_json_transaction();

    let appservice = appservice(None).await?;
    let client = appservice.get_cached_client(None)?;

    let room = client.get_joined_room(&room_id);
    assert!(room.is_none());

    #[cfg(feature = "warp")]
    let status = warp::test::request()
        .method("PUT")
        .path(uri)
        .json(&transaction)
        .filter(&appservice.warp_filter())
        .await
        .unwrap()
        .into_response()
        .status();

    #[cfg(feature = "actix")]
    let status = {
        let app =
            actix_test::init_service(ActixApp::new().configure(appservice.actix_configure())).await;

        let req = actix_test::TestRequest::put().uri(uri).set_json(&transaction).to_request();

        actix_test::call_service(&app, req).await.status()
    };

    assert_eq!(status, 200);

    let room = client.get_left_room(&room_id);
    assert!(room.is_none());

    let room = client.get_joined_room(&room_id);
    assert!(room.is_some());

    Ok(())
}

#[async_test]
async fn test_client_join_room() -> Result<()> {
    let appservice = appservice(None).await?;
    let client = appservice.get_cached_client(None)?;
    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

    let room = client.get_joined_room(&room_id);
    assert!(room.is_none());

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/join$".to_string()))
        .with_status(200)
        .with_body(json!({ "room_id": room_id }).to_string())
        .match_header("authorization", "Bearer as_token")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/state$".to_string()))
        .with_status(200)
        .match_header("authorization", "Bearer as_token")
        .with_body(test_json::ROOM_STATE.to_string())
        .create();

    appservice.join_room_by_id(None, &room_id).await?;

    let room = client.get_left_room(&room_id);
    assert!(room.is_none());

    let room = client.get_joined_room(&room_id);
    assert!(room.is_some());

    Ok(())
}

#[async_test]
async fn test_virtual_user_client_join_room_by_id() -> Result<()> {
    let appservice = appservice(None).await?;
    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    let virtual_user_id = user_id!("@virtual_user:localhost");

    let _m = mockito::mock("POST", Matcher::Regex(r"^/_matrix/client/r0/register\??$".to_owned()))
        .with_body(serde_json::to_string(&json!(
            {
                "access_token": "abc123",
                "device_id": "GHTYAJCE",
                "user_id": "@virtual_user:localhost"
            }
        ))?)
        .create();

    let _m = mock(
        "POST",
        Matcher::Regex(format!(
            r"^/_matrix/client/r0/rooms/{}/join\?user_id={}",
            encode(&room_id.to_string()),
            encode(&virtual_user_id.to_string())
        )),
    )
    .with_status(200)
    .with_body(json!({ "room_id": room_id }).to_string())
    .match_header("authorization", "Bearer as_token")
    .create();

    let _m = mock(
        "GET",
        Matcher::Regex(format!(
            r"^/_matrix/client/r0/rooms/{}/state\?user_id={}",
            encode(&room_id.to_string()),
            encode(&virtual_user_id.to_string())
        )),
    )
    .with_status(200)
    .match_header("authorization", "Bearer as_token")
    .with_body(test_json::ROOM_STATE.to_string())
    .create();

    appservice.join_room_by_id(Some(virtual_user_id.localpart()), &room_id).await?;

    Ok(())
}

#[async_test]
async fn test_virtual_user_client_join_room_by_id_or_alias() -> Result<()> {
    let appservice = appservice(None).await?;
    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    let room_alias_id = RoomIdOrAliasId::try_from("#testroom:localhost")?;
    let virtual_user_id = user_id!("@virtual_user:localhost");

    let _m = mockito::mock("POST", Matcher::Regex(r"^/_matrix/client/r0/register\??$".to_owned()))
        .with_body(serde_json::to_string(&json!(
            {
                "access_token": "abc123",
                "device_id": "GHTYAJCE",
                "user_id": "@virtual_user:localhost"
            }
        ))?)
        .create();

    let _m = mock(
        "POST",
        Matcher::Regex(format!(
            r"^/_matrix/client/r0/join/{}\?&?user_id={}",
            encode(&room_alias_id.to_string()),
            encode(&virtual_user_id.to_string())
        )),
    )
    .with_status(200)
    .with_body(json!({ "room_id": room_id }).to_string())
    .match_header("authorization", "Bearer as_token")
    .create();

    let _m = mock(
        "GET",
        Matcher::Regex(format!(
            r"^/_matrix/client/r0/rooms/{}/state\?user_id={}",
            encode(&room_id.to_string()),
            encode(&virtual_user_id.to_string())
        )),
    )
    .with_status(200)
    .match_header("authorization", "Bearer as_token")
    .with_body(test_json::ROOM_STATE.to_string())
    .create();

    appservice
        .join_room_by_id_or_alias(Some(virtual_user_id.localpart()), &room_alias_id, &[])
        .await?;

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
