use std::time::Duration;

use assert_matches::assert_matches;
use futures_util::StreamExt;
use matrix_sdk::{
    config::RequestConfig,
    executor::spawn,
    matrix_auth::{Session, SessionTokens},
    HttpError, RefreshTokenError,
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::{
        client::{account::register, error::ErrorKind},
        MatrixVersion,
    },
    assign, device_id, user_id,
};
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::{
    matchers::{body_partial_json, header, method, path},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, no_retry_test_client, test_client_builder};

fn session() -> Session {
    Session {
        meta: SessionMeta {
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: SessionTokens {
            access_token: "1234".to_owned(),
            refresh_token: Some("abcd".to_owned()),
        },
    }
}

#[async_test]
async fn login_username_refresh_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .and(body_partial_json(json!({
            "refresh_token": true,
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_REFRESH_TOKEN),
        )
        .mount(&server)
        .await;

    let res = client
        .matrix_auth()
        .login_username("example", "wordpass")
        .request_refresh_token()
        .send()
        .await
        .unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
    res.refresh_token.unwrap();
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn login_sso_refresh_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .and(body_partial_json(json!({
            "refresh_token": true,
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_REFRESH_TOKEN),
        )
        .mount(&server)
        .await;

    let idp = ruma::api::client::session::get_login_types::v3::IdentityProvider::new(
        "some-id".to_owned(),
        "idp-name".to_owned(),
    );
    let res = client
        .matrix_auth()
        .login_sso(|sso_url| async move {
            let sso_url = url::Url::parse(&sso_url).unwrap();

            let (_, redirect) =
                sso_url.query_pairs().find(|(key, _)| key == "redirectUrl").unwrap();

            let mut redirect_url = url::Url::parse(&redirect).unwrap();
            redirect_url.set_query(Some("loginToken=tinytoken"));

            reqwest::get(redirect_url.to_string()).await.unwrap();

            Ok(())
        })
        .identity_provider_id(&idp.id)
        .request_refresh_token()
        .await
        .unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
    res.refresh_token.unwrap();
}

#[async_test]
async fn register_refresh_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/register"))
        .and(body_partial_json(json!({
            "refresh_token": true,
        })))
        .respond_with(
            // Successful registration response is the same as for login,
            // if `inhibit_login` is `false`.
            ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_REFRESH_TOKEN),
        )
        .mount(&server)
        .await;

    let req = assign!(register::v3::Request::new(), {
        username: Some("user".to_owned()),
        password: Some("password".to_owned()),
        auth: None,
        refresh_token: true,
    });

    let res = client.matrix_auth().register(req).await.unwrap();

    res.refresh_token.unwrap();
}

#[async_test]
async fn no_refresh_token() {
    let (client, server) = logged_in_client().await;

    // Refresh token doesn't change.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .expect(0)
        .mount(&server)
        .await;

    let res = client.refresh_access_token().await;
    assert_matches!(res, Err(HttpError::RefreshToken(RefreshTokenError::RefreshTokenRequired)));
}

#[async_test]
async fn refresh_token() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    let tokens = auth.session_tokens().unwrap();
    assert_eq!(tokens.access_token, "1234");
    assert_eq!(tokens.refresh_token.as_deref(), Some("abcd"));

    // Refresh token doesn't change.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .and(body_partial_json(json!({
            "refresh_token": "abcd",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    auth.refresh_access_token().await.unwrap();
    let tokens = auth.session_tokens().unwrap();
    assert_eq!(tokens.access_token, "5678");
    assert_eq!(tokens.refresh_token.as_deref(), Some("abcd"));

    // Refresh token changes.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .and(body_partial_json(json!({
            "refresh_token": "abcd",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN_WITH_REFRESH_TOKEN),
        )
        .mount(&server)
        .await;

    auth.refresh_access_token().await.unwrap();
    let tokens = auth.session_tokens().unwrap();
    assert_eq!(tokens.access_token, "9012");
    assert_eq!(tokens.refresh_token.as_deref(), Some("wxyz"));
}

#[async_test]
async fn refresh_token_not_handled() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .mount(&server)
        .await;

    let res = client.whoami().await.unwrap_err();
    assert_matches!(res.client_api_error_kind(), Some(ErrorKind::UnknownToken { .. }));
}

#[async_test]
async fn refresh_token_handled_success() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    let mut tokens_stream = auth.session_tokens_stream().unwrap();
    let tokens_join_handle = spawn(async move {
        let tokens = tokens_stream.next().await.unwrap();
        assert_eq!(tokens.access_token, "5678");
        assert_eq!(tokens.refresh_token.as_deref(), Some("abcd"));
    });

    let mut tokens_changed_stream = auth.session_tokens_changed_stream().unwrap();
    let changed_join_handle = spawn(async move {
        tokens_changed_stream.next().await.unwrap();
    });

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .expect(1)
        .named("`POST /refresh`")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .expect(1)
        .named("`GET /whoami` wrong token")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 5678"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .expect(1)
        .named("`GET /whoami` good token")
        .mount(&server)
        .await;

    client.whoami().await.unwrap();
    tokens_join_handle.await.unwrap();
    changed_join_handle.await.unwrap();
}

#[async_test]
async fn refresh_token_handled_failure() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .expect(1)
        .named("`POST /refresh`")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .expect(1)
        .named("`GET /whoami` wrong token")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 5678"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .expect(0)
        .named("`GET /whoami` good token")
        .mount(&server)
        .await;

    let res = client.whoami().await.unwrap_err();
    assert_matches!(res.client_api_error_kind(), Some(ErrorKind::UnknownToken { .. }))
}

#[async_test]
async fn refresh_token_handled_multi_success() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&*test_json::REFRESH_TOKEN)
                .set_delay(Duration::from_secs(1)),
        )
        .expect(1)
        .named("`POST /refresh`")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .expect(3)
        .named("`GET /whoami` wrong token")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 5678"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .expect(3)
        .named("`GET /whoami` good token")
        .mount(&server)
        .await;

    let (sender, mut receiver) = mpsc::channel::<()>(3);
    let client_clone = client.clone();
    let sender_clone = sender.clone();
    spawn(async move {
        client_clone.whoami().await.unwrap();
        sender_clone.try_send(()).unwrap();
    });
    let client_clone = client.clone();
    let sender_clone = sender.clone();
    spawn(async move {
        client_clone.whoami().await.unwrap();
        sender_clone.try_send(()).unwrap();
    });
    spawn(async move {
        client.whoami().await.unwrap();
        sender.try_send(()).unwrap();
    });

    let mut i = 0;
    while i < 3 {
        if receiver.recv().await.is_some() {
            i += 1;
        }
    }
}

#[async_test]
async fn refresh_token_handled_multi_failure() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT)
                .set_delay(Duration::from_secs(1)),
        )
        .expect(1)
        .named("`POST /refresh`")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(&*test_json::UNKNOWN_TOKEN_SOFT_LOGOUT),
        )
        .expect(3)
        .named("`GET /whoami` wrong token")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .and(header(http::header::AUTHORIZATION, "Bearer 5678"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .expect(0)
        .named("`GET /whoami` good token")
        .mount(&server)
        .await;

    let (sender, mut receiver) = mpsc::channel::<()>(3);
    let client_clone = client.clone();
    let sender_clone = sender.clone();
    spawn(async move {
        client_clone.whoami().await.unwrap_err();
        sender_clone.try_send(()).unwrap();
    });
    let client_clone = client.clone();
    let sender_clone = sender.clone();
    spawn(async move {
        client_clone.whoami().await.unwrap_err();
        sender_clone.try_send(()).unwrap();
    });
    spawn(async move {
        client.whoami().await.unwrap_err();
        sender.try_send(()).unwrap();
    });

    let mut i = 0;
    while i < 3 {
        if receiver.recv().await.is_some() {
            i += 1;
        }
    }
}

#[async_test]
async fn refresh_token_handled_other_error() {
    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let session = session();
    auth.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .expect(0)
        .named("`POST /refresh`")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .respond_with(ResponseTemplate::new(404).set_body_json(&*test_json::NOT_FOUND))
        .expect(1)
        .named("`GET /whoami`")
        .mount(&server)
        .await;

    client.whoami().await.unwrap_err();
}
