use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use futures_util::StreamExt;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::RequestConfig,
    executor::spawn,
    test_utils::{
        client::mock_session_meta, logged_in_client_with_server, no_retry_test_client_with_server,
        test_client_builder_with_server,
    },
    HttpError, RefreshTokenError, SessionChange, SessionTokens,
};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::{
        client::{account::register, error::ErrorKind},
        MatrixVersion,
    },
    assign,
};
use serde_json::json;
use tokio::sync::{broadcast::error::TryRecvError, mpsc};
use wiremock::{
    matchers::{body_partial_json, header, method, path},
    Mock, ResponseTemplate,
};

fn session() -> MatrixSession {
    MatrixSession {
        meta: mock_session_meta(),
        tokens: SessionTokens {
            access_token: "1234".to_owned(),
            refresh_token: Some("abcd".to_owned()),
        },
    }
}

#[async_test]
async fn test_login_username_refresh_token() {
    let (client, server) = no_retry_test_client_with_server().await;

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
async fn test_login_sso_refresh_token() {
    let (client, server) = no_retry_test_client_with_server().await;

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
async fn test_register_refresh_token() {
    let (client, server) = no_retry_test_client_with_server().await;

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
async fn test_no_refresh_token() {
    let (client, server) = logged_in_client_with_server().await;

    // Refresh token doesn't change.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::REFRESH_TOKEN))
        .expect(0)
        .mount(&server)
        .await;

    let res = client.refresh_access_token().await;
    assert_matches!(res, Err(RefreshTokenError::RefreshTokenRequired));
}

#[async_test]
async fn test_refresh_token() {
    let (builder, server) = test_client_builder_with_server().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .server_versions([MatrixVersion::V1_3])
        .build()
        .await
        .unwrap();
    let auth = client.matrix_auth();

    let num_save_session_callback_calls = Arc::new(Mutex::new(0));
    client
        .set_session_callbacks(Box::new(|_| panic!("reload session never called")), {
            let num_save_session_callback_calls = num_save_session_callback_calls.clone();
            Box::new(move |_client| {
                *num_save_session_callback_calls.lock().unwrap() += 1;
                Ok(())
            })
        })
        .unwrap();

    let mut session_changes = client.subscribe_to_session_changes();

    let session = session();
    auth.restore_session(session).await.unwrap();

    assert_eq!(*num_save_session_callback_calls.lock().unwrap(), 0);
    assert_eq!(session_changes.try_recv(), Err(TryRecvError::Empty));

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
    assert_eq!(*num_save_session_callback_calls.lock().unwrap(), 1);
    assert_eq!(session_changes.try_recv(), Ok(SessionChange::TokensRefreshed));

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
    assert_eq!(*num_save_session_callback_calls.lock().unwrap(), 2);
    assert_eq!(session_changes.try_recv(), Ok(SessionChange::TokensRefreshed));

    assert_eq!(session_changes.try_recv(), Err(TryRecvError::Empty));
}

#[async_test]
async fn test_refresh_token_not_handled() {
    let (builder, server) = test_client_builder_with_server().await;
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
async fn test_refresh_token_handled_success() {
    let (builder, server) = test_client_builder_with_server().await;
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

    let mut session_changes = client.subscribe_to_session_changes();

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

    assert_eq!(session_changes.try_recv(), Ok(SessionChange::TokensRefreshed));
    assert_eq!(session_changes.try_recv(), Err(TryRecvError::Empty));
}

#[async_test]
async fn test_refresh_token_handled_failure() {
    let (builder, server) = test_client_builder_with_server().await;
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

    let mut session_changes = client.subscribe_to_session_changes();

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

    let res = client.whoami().await;
    assert_let!(Err(HttpError::RefreshToken(RefreshTokenError::MatrixAuth(http_err))) = res);
    assert_matches!(
        http_err.client_api_error_kind(),
        Some(ErrorKind::UnknownToken { soft_logout: true })
    );

    assert_eq!(session_changes.try_recv(), Ok(SessionChange::UnknownToken { soft_logout: true }));
    assert_eq!(session_changes.try_recv(), Err(TryRecvError::Empty));
}

#[async_test]
async fn test_refresh_token_handled_multi_success() {
    let (builder, server) = test_client_builder_with_server().await;
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
async fn test_refresh_token_handled_multi_failure() {
    let (builder, server) = test_client_builder_with_server().await;
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
async fn test_refresh_token_handled_other_error() {
    let (builder, server) = test_client_builder_with_server().await;
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

#[cfg(feature = "experimental-oidc")]
#[async_test]
async fn test_oauth_refresh_token_handled_success() {
    use matrix_sdk::{
        assert_next_eq_with_timeout,
        test_utils::{
            client::{
                mock_prev_session_tokens_with_refresh, mock_session_tokens_with_refresh,
                oauth::mock_session,
            },
            mocks::MatrixMockServer,
        },
    };

    let server = MatrixMockServer::new().await;
    // Return an error first so the token is refreshed.
    server
        .mock_who_am_i()
        .expected_access_token("prev-access-token")
        .err_unknown_token()
        .expect(1)
        .named("whoami_unknown_token")
        .mount()
        .await;
    server.mock_who_am_i().ok().expect(1).named("whoami_ok").mount().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
    oauth_server.mock_token().ok().expect(1).named("token").mount().await;

    let issuer = server.server().uri();
    let client = server.client_builder().unlogged().handle_refresh_tokens().build().await;
    let oidc = client.oidc();

    oidc.restore_session(mock_session(mock_prev_session_tokens_with_refresh(), issuer))
        .await
        .unwrap();

    let mut tokens_stream = oidc.session_tokens_stream().unwrap();
    let mut session_changes = client.subscribe_to_session_changes();

    client.whoami().await.unwrap();

    // We receive the new tokens on the stream.
    assert_next_eq_with_timeout!(
        tokens_stream,
        mock_session_tokens_with_refresh(),
        "The session tokens stream should receive the updated tokens"
    );

    // We get notified once that the tokens were refreshed.
    assert_eq!(
        session_changes.try_recv(),
        Ok(SessionChange::TokensRefreshed),
        "The session changes should be notified of the tokens refresh"
    );
    assert_eq!(
        session_changes.try_recv(),
        Err(TryRecvError::Empty),
        "There should be no more session changes"
    );
}

#[cfg(feature = "experimental-oidc")]
#[async_test]
async fn test_oauth_refresh_token_handled_failure() {
    use matrix_sdk::{
        authentication::oidc::OidcError,
        test_utils::{
            client::{mock_prev_session_tokens_with_refresh, oauth::mock_session},
            mocks::MatrixMockServer,
        },
    };

    let server = MatrixMockServer::new().await;
    // Return an error first so the token is refreshed.
    server
        .mock_who_am_i()
        .expected_access_token("prev-access-token")
        .err_unknown_token()
        .expect(1)
        .named("whoami_unknown_token")
        .mount()
        .await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
    // Return an error to fail the token refresh.
    oauth_server.mock_token().invalid_grant().expect(1).named("token").mount().await;

    let issuer = server.server().uri();
    let client = server.client_builder().unlogged().handle_refresh_tokens().build().await;
    let oidc = client.oidc();

    oidc.restore_session(mock_session(mock_prev_session_tokens_with_refresh(), issuer))
        .await
        .unwrap();

    let mut session_changes = client.subscribe_to_session_changes();

    // The request fails with a refresh token error.
    let res = client.whoami().await;
    assert_let!(
        Err(HttpError::RefreshToken(RefreshTokenError::Oidc(oidc_err))) = res,
        "The request should fail with a refresh token error from the OIDC API"
    );
    assert_matches!(
        *oidc_err,
        OidcError::RefreshToken(_),
        "The OIDC error should be a refresh token error"
    );

    // We get notified once that the token is invalid.
    assert_eq!(
        session_changes.try_recv(),
        Ok(SessionChange::UnknownToken { soft_logout: false }),
        "The session changes should be notified that the token is invalid"
    );
    assert_eq!(
        session_changes.try_recv(),
        Err(TryRecvError::Empty),
        "There should be no more session changes"
    );
}

#[cfg(feature = "experimental-oidc")]
#[async_test]
async fn test_oauth_handle_refresh_tokens() {
    use matrix_sdk::test_utils::{
        client::{
            mock_prev_session_tokens_with_refresh, mock_session_tokens_with_refresh,
            oauth::mock_session,
        },
        mocks::MatrixMockServer,
    };

    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();

    oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;

    let issuer = server.server().uri();
    let client = server.client_builder().unlogged().handle_refresh_tokens().build().await;

    let oidc = client.oidc();
    oidc.restore_session(mock_session(mock_prev_session_tokens_with_refresh(), issuer))
        .await
        .unwrap();

    let num_save_session_callback_calls = Arc::new(Mutex::new(0));
    client
        .set_session_callbacks(Box::new(|_| panic!("reload session never called")), {
            let num_save_session_callback_calls = num_save_session_callback_calls.clone();
            Box::new(move |_client| {
                *num_save_session_callback_calls.lock().unwrap() += 1;
                Ok(())
            })
        })
        .unwrap();

    let mut session_changes = client.subscribe_to_session_changes();
    assert_eq!(session_changes.try_recv(), Err(TryRecvError::Empty));

    assert_eq!(oidc.session_tokens(), Some(mock_prev_session_tokens_with_refresh()));

    // Refresh token successfully.
    oauth_server.mock_token().ok().mock_once().named("refresh_token").mount().await;

    oidc.refresh_access_token().await.unwrap();

    // The tokens were updated.
    assert_eq!(
        oidc.session_tokens(),
        Some(mock_session_tokens_with_refresh()),
        "The session tokens should have been updated with the new values"
    );

    // The save session callback was called once.
    assert_eq!(
        *num_save_session_callback_calls.lock().unwrap(),
        1,
        "The save session callback should have been called once"
    );

    // We get notified once that the tokens were refreshed.
    assert_eq!(
        session_changes.try_recv(),
        Ok(SessionChange::TokensRefreshed),
        "The session changes should be notified of the tokens refresh"
    );
    assert_eq!(
        session_changes.try_recv(),
        Err(TryRecvError::Empty),
        "There should be no more session changes"
    );
}
