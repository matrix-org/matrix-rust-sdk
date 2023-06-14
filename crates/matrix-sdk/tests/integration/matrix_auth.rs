use matrix_sdk::RumaApiError;
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::client::{
        self as client_api,
        account::register::{v3::Request as RegistrationRequest, RegistrationKind},
        session::get_login_types::v3::LoginType,
        uiaa,
    },
    assign,
};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

use crate::no_retry_test_client;

#[async_test]
async fn login() {
    let (client, server) = no_retry_test_client().await;
    let homeserver = Url::parse(&server.uri()).unwrap();

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

    let can_password = client
        .matrix_auth()
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Password(_)));
    assert!(can_password);

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, homeserver);
}

#[async_test]
async fn login_with_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_DISCOVERY))
        .mount(&server)
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await.as_str(), "https://example.org/");
}

#[async_test]
async fn login_no_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, Url::parse(&server.uri()).unwrap());
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn login_with_sso() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    let idp = ruma::api::client::session::get_login_types::v3::IdentityProvider::new(
        "some-id".to_owned(),
        "idp-name".to_owned(),
    );
    client
        .matrix_auth()
        .login_sso(|sso_url| async move {
            let sso_url = Url::parse(&sso_url).unwrap();

            let (_, redirect) =
                sso_url.query_pairs().find(|(key, _)| key == "redirectUrl").unwrap();

            let mut redirect_url = Url::parse(&redirect).unwrap();
            redirect_url.set_query(Some("loginToken=tinytoken"));

            reqwest::get(redirect_url.to_string()).await.unwrap();

            Ok(())
        })
        .identity_provider_id(&idp.id)
        .await
        .unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_with_sso_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

    let auth = client.matrix_auth();
    let can_sso = auth
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Sso(_)));
    assert!(can_sso);

    let sso_url = auth.get_sso_login_url("http://127.0.0.1:3030", None).await;
    sso_url.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    auth.login_token("averysmalltoken").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(403).set_body_json(&*test_json::LOGIN_RESPONSE_ERR))
        .mount(&server)
        .await;

    if let Err(err) = client.matrix_auth().login_username("example", "wordpass").send().await {
        if let Some(RumaApiError::ClientApi(client_api::Error { status_code, body })) =
            err.as_ruma_api_error()
        {
            assert_eq!(*status_code, http::StatusCode::from_u16(403).unwrap());

            if let client_api::error::ErrorBody::Standard { kind, message } = body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:?}, expected `Error::RumaResponse");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn register_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/register"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(&*test_json::REGISTRATION_RESPONSE_ERR),
        )
        .mount(&server)
        .await;

    let user = assign!(RegistrationRequest::new(), {
        username: Some("user".to_owned()),
        password: Some("password".to_owned()),
        auth: Some(uiaa::AuthData::FallbackAcknowledgement(
            uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
        )),
        kind: RegistrationKind::User,
    });

    if let Err(err) = client.matrix_auth().register(user).await {
        if let Some(client_api::Error { status_code, body }) = err.as_client_api_error() {
            assert_eq!(*status_code, http::StatusCode::from_u16(403).unwrap());
            if let client_api::error::ErrorBody::Standard { kind, message } = body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:#?}, expected `UiaaResponse`");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}
