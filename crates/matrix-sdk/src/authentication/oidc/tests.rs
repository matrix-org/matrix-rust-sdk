use std::collections::HashMap;

use anyhow::Context as _;
use assert_matches::assert_matches;
use mas_oidc_client::{
    requests::{
        account_management::AccountManagementActionFull,
        authorization_code::AuthorizationValidationData,
    },
    types::{errors::ClientErrorCode, registration::VerifiedClientMetadata, requests::Prompt},
};
use matrix_sdk_test::async_test;
use ruma::ServerName;
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tempfile::tempdir;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

use super::{
    registrations::OidcRegistrations, AuthorizationCode, AuthorizationError, AuthorizationResponse,
    Oidc, OidcError, OidcSessionTokens, RedirectUriQueryParseError,
};
use crate::{
    test_utils::{
        client::{
            oauth::{mock_client_metadata, mock_session, mock_session_tokens},
            MockClientBuilder,
        },
        mocks::{oauth::MockServerMetadataBuilder, MatrixMockServer},
    },
    Client, Error,
};

const REDIRECT_URI_STRING: &str = "http://127.0.0.1:6778/oidc/callback";

/// Different session tokens than the ones returned by the mock server's
/// token endpoint.
pub(crate) fn prev_session_tokens() -> OidcSessionTokens {
    OidcSessionTokens {
        access_token: "prev-access-token".to_owned(),
        refresh_token: Some("prev-refresh-token".to_owned()),
    }
}

async fn mock_environment(
) -> anyhow::Result<(Oidc, MatrixMockServer, VerifiedClientMetadata, OidcRegistrations)> {
    let server = MatrixMockServer::new().await;
    server.mock_who_am_i().ok().named("whoami").mount().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
    oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;
    oauth_server.mock_token().ok().mount().await;

    let client = server.client_builder().unlogged().build().await;
    let client_metadata = mock_client_metadata();

    let registrations_path = tempdir().unwrap().path().join("oidc").join("registrations.json");
    let registrations =
        OidcRegistrations::new(&registrations_path, client_metadata.clone(), HashMap::new())
            .unwrap();

    Ok((client.oidc(), server, client_metadata, registrations))
}

#[async_test]
async fn test_high_level_login() -> anyhow::Result<()> {
    // Given a fresh environment.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    assert!(oidc.issuer().is_none());
    assert!(oidc.client_id().is_none());

    // When getting the OIDC login URL.
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    // Then the client should be configured correctly.
    assert!(oidc.issuer().is_some());
    assert!(oidc.client_id().is_some());

    // When completing the login with a valid callback.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri.set_query(Some(&format!("code=42&state={}", authorization_data.state)));

    // Then the login should succeed.
    oidc.login_with_oidc_callback(&authorization_data, callback_uri).await?;

    Ok(())
}

#[async_test]
async fn test_high_level_login_cancellation() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    assert!(oidc.issuer().is_some());
    assert!(oidc.client_id().is_some());

    // When completing login with a cancellation callback.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri
        .set_query(Some(&format!("error=access_denied&state={}", authorization_data.state)));

    let error = oidc.login_with_oidc_callback(&authorization_data, callback_uri).await.unwrap_err();

    // Then a cancellation error should be thrown.
    assert_matches!(error, Error::Oidc(OidcError::CancelledAuthorization));

    Ok(())
}

#[async_test]
async fn test_high_level_login_invalid_state() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    assert!(oidc.issuer().is_some());
    assert!(oidc.client_id().is_some());

    // When completing login with an old/tampered state.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri.set_query(Some("code=42&state=imposter_alert"));

    let error = oidc.login_with_oidc_callback(&authorization_data, callback_uri).await.unwrap_err();

    // Then the login should fail by flagging the invalid state.
    assert_matches!(error, Error::Oidc(OidcError::InvalidState));

    Ok(())
}

#[async_test]
async fn test_login() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let issuer = server.server().uri();

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).mount().await;

    let client = server.client_builder().registered_with_oauth(server.server().uri()).build().await;
    let oidc = client.oidc();

    let device_id = "D3V1C31D".to_owned(); // yo this is 1999 speaking

    let redirect_uri_str = REDIRECT_URI_STRING;
    let redirect_uri = Url::parse(redirect_uri_str)?;
    let authorization_data = oidc.login(redirect_uri, Some(device_id.clone()))?.build().await?;

    tracing::debug!("authorization data URL = {}", authorization_data.url);

    let mut num_expected = 6;
    let mut nonce = None;

    for (key, val) in authorization_data.url.query_pairs() {
        match &*key {
            "response_type" => {
                assert_eq!(val, "code");
                num_expected -= 1;
            }
            "client_id" => {
                assert_eq!(val, "test_client_id");
                num_expected -= 1;
            }
            "redirect_uri" => {
                assert_eq!(val, redirect_uri_str);
                num_expected -= 1;
            }
            "scope" => {
                assert_eq!(val, format!("openid urn:matrix:org.matrix.msc2967.client:api:* urn:matrix:org.matrix.msc2967.client:device:{device_id}"));
                num_expected -= 1;
            }
            "state" => {
                num_expected -= 1;
                assert_eq!(val, authorization_data.state);
            }
            "nonce" => {
                num_expected -= 1;
                nonce = Some(val);
            }
            _ => panic!("unexpected query parameter: {key}={val}"),
        }
    }

    assert_eq!(num_expected, 0);

    let data = oidc.data().unwrap();
    let authorization_data_guard = data.authorization_data.lock().await;

    let state = authorization_data_guard.get(&authorization_data.state).context("missing state")?;
    let nonce = nonce.context("missing nonce")?;
    assert_eq!(nonce, state.nonce);

    assert!(authorization_data.url.as_str().starts_with(&issuer));
    assert_eq!(authorization_data.url.path(), "/oauth2/authorize");

    Ok(())
}

#[test]
fn test_authorization_response() -> anyhow::Result<()> {
    let uri = Url::parse("https://example.com")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Err(RedirectUriQueryParseError::MissingQuery)
    );

    let uri = Url::parse("https://example.com?code=123&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Ok(AuthorizationResponse::Success(AuthorizationCode { code, state })) => {
            assert_eq!(code, "123");
            assert_eq!(state, "456");
        }
    );

    let uri = Url::parse("https://example.com?error=invalid_grant&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Ok(AuthorizationResponse::Error(AuthorizationError { error, state })) => {
            assert_eq!(error.error, ClientErrorCode::InvalidGrant);
            assert_eq!(error.error_description, None);
            assert_eq!(state, "456");
        }
    );

    Ok(())
}

#[async_test]
async fn test_finish_authorization() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();

    oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
    oauth_server.mock_token().ok().expect(1).named("token").mount().await;

    let client = server.client_builder().registered_with_oauth(server.server().uri()).build().await;
    let oidc = client.oidc();

    // If the state is missing, then any attempt to finish authorizing will fail.
    let res = oidc
        .finish_authorization(AuthorizationCode { code: "42".to_owned(), state: "none".to_owned() })
        .await;

    assert_matches!(res, Err(OidcError::InvalidState));
    assert!(oidc.session_tokens().is_none());

    // Assuming a non-empty state "123"...
    let state = "state".to_owned();
    let redirect_uri = REDIRECT_URI_STRING;
    let auth_validation_data = AuthorizationValidationData {
        state: state.clone(),
        nonce: "nonce".to_owned(),
        redirect_uri: Url::parse(redirect_uri)?,
        code_challenge_verifier: None,
    };

    {
        let data = oidc.data().context("missing data")?;
        let prev = data.authorization_data.lock().await.insert(state.clone(), {
            AuthorizationValidationData { ..auth_validation_data.clone() }
        });
        assert!(prev.is_none());
    }

    // Finishing the authorization for another state won't work.
    let res = oidc
        .finish_authorization(AuthorizationCode {
            code: "1337".to_owned(),
            state: "none".to_owned(),
        })
        .await;

    assert_matches!(res, Err(OidcError::InvalidState));
    assert!(oidc.session_tokens().is_none());
    assert!(oidc.data().unwrap().authorization_data.lock().await.get(&state).is_some());

    // Finishing the authorization for the expected state will work.
    oidc.finish_authorization(AuthorizationCode { code: "1337".to_owned(), state: state.clone() })
        .await?;

    assert!(oidc.session_tokens().is_some());
    assert!(oidc.data().unwrap().authorization_data.lock().await.get(&state).is_none());

    Ok(())
}

#[async_test]
async fn test_oidc_session() -> anyhow::Result<()> {
    let client = MockClientBuilder::new("https://example.org".to_owned()).unlogged().build().await;
    let oidc = client.oidc();

    let tokens = mock_session_tokens();
    let issuer = "https://oidc.example.com/issuer";
    let session = mock_session(tokens.clone(), issuer.to_owned());
    oidc.restore_session(session.clone()).await?;

    // Test a few extra getters.
    assert_eq!(oidc.access_token().unwrap(), tokens.access_token);
    assert_eq!(oidc.refresh_token(), tokens.refresh_token);

    let user_session = oidc.user_session().unwrap();
    assert_eq!(user_session.meta, session.user.meta);
    assert_eq!(user_session.tokens, tokens);
    assert_eq!(user_session.issuer, issuer);

    let full_session = oidc.full_session().unwrap();

    assert_eq!(full_session.client_id.0, "test_client_id");
    assert_eq!(full_session.user.meta, session.user.meta);
    assert_eq!(full_session.user.tokens, tokens);
    assert_eq!(full_session.user.issuer, issuer);

    Ok(())
}

#[async_test]
async fn test_insecure_clients() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let server_url = server.server().uri();

    server.mock_well_known().ok().expect(1).named("well_known").mount().await;
    server.mock_versions().ok().expect(1..).named("versions").mount().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(2..).named("server_metadata").mount().await;
    oauth_server.mock_token().ok().expect(2).named("token").mount().await;

    let prev_tokens = prev_session_tokens();
    let next_tokens = mock_session_tokens();

    for client in [
        // Create an insecure client with the homeserver_url method.
        Client::builder().homeserver_url(&server_url).build().await?,
        // Create an insecure client with the insecure_server_name_no_tls method.
        Client::builder()
            .insecure_server_name_no_tls(&ServerName::parse(
                server_url.strip_prefix("http://").unwrap(),
            )?)
            .build()
            .await?,
    ] {
        let oidc = client.oidc();

        // Restore the previous session so we have an existing set of refresh tokens.
        oidc.restore_session(mock_session(prev_tokens.clone(), server_url.clone())).await?;

        let mut session_token_stream = oidc.session_tokens_stream().expect("stream available");

        assert_pending!(session_token_stream);

        // A refresh in insecure mode should work Just Fine.
        oidc.refresh_access_token().await?;

        assert_next_matches!(session_token_stream, new_tokens => {
            assert_eq!(new_tokens, next_tokens);
        });

        assert_pending!(session_token_stream);
    }

    Ok(())
}

#[async_test]
async fn test_register_client() {
    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();
    let client = server.client_builder().unlogged().build().await;
    let oidc = client.oidc();
    let client_metadata = mock_client_metadata();

    // Server doesn't support registration, it fails.
    oauth_server
        .mock_server_metadata()
        .ok_without_registration()
        .expect(1)
        .named("metadata_without_registration")
        .mount()
        .await;

    let result = oidc.register_client(client_metadata.clone(), None).await;
    assert_matches!(result, Err(OidcError::NoRegistrationSupport));

    server.verify_and_reset().await;

    // Server supports registration, it succeeds.
    oauth_server
        .mock_server_metadata()
        .ok()
        .expect(1)
        .named("metadata_with_registration")
        .mount()
        .await;
    oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;

    let response = oidc.register_client(client_metadata, None).await.unwrap();
    assert_eq!(response.client_id, "test_client_id");

    let auth_data = oidc.data().unwrap();
    // There is a difference of ending slash between the strings so we parse them
    // with `Url` which will normalize that.
    assert_eq!(Url::parse(&auth_data.issuer), Url::parse(&server.server().uri()));
    assert_eq!(auth_data.client_id.0, response.client_id);
}

#[async_test]
async fn test_management_url_cache() {
    let server = MatrixMockServer::new().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).mount().await;

    let client = server.client_builder().logged_in_with_oauth(server.server().uri()).build().await;
    let oidc = client.oidc();

    // The cache should not contain the entry.
    assert!(!client.inner.caches.provider_metadata.lock().await.contains("PROVIDER_METADATA"));

    let management_url = oidc
        .account_management_url(Some(AccountManagementActionFull::Profile))
        .await
        .expect("We should be able to fetch the account management url");

    assert!(management_url.is_some());

    // Check that the provider metadata has been inserted into the cache.
    assert!(client.inner.caches.provider_metadata.lock().await.contains("PROVIDER_METADATA"));

    // Another parameter doesn't make another request for the metadata.
    let management_url = oidc
        .account_management_url(Some(AccountManagementActionFull::SessionsList))
        .await
        .expect("We should be able to fetch the account management url");

    assert!(management_url.is_some());
}

#[async_test]
async fn test_provider_metadata() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;
    let oidc = client.oidc();
    let issuer = server.server().uri();

    // The endpoint is not mocked so it is not supported.
    let error = oidc.provider_metadata().await.unwrap_err();
    assert!(error.is_not_supported());

    // Mock the `GET /auth_issuer` fallback endpoint.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc2965/auth_issuer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer": issuer})))
        .expect(1)
        .named("auth_issuer")
        .mount(server.server())
        .await;
    let metadata = MockServerMetadataBuilder::new(&issuer).build();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .expect(1)
        .named("openid-configuration")
        .mount(server.server())
        .await;
    oidc.provider_metadata().await.unwrap();

    // Mock the `GET /auth_metadata` endpoint.
    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).named("auth_metadata").mount().await;

    oidc.provider_metadata().await.unwrap();
}
