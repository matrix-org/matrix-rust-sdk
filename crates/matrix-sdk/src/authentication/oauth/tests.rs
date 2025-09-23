use anyhow::Context as _;
use assert_matches::assert_matches;
use matrix_sdk_base::store::RoomLoadSettings;
use matrix_sdk_test::async_test;
use oauth2::{ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope};
use ruma::{
    DeviceId, ServerName, api::client::discovery::get_authorization_server_metadata::v1::Prompt,
    device_id, owned_device_id, user_id,
};
use tokio::sync::broadcast::error::TryRecvError;
use url::Url;

use super::{
    AuthorizationCode, AuthorizationError, AuthorizationResponse, OAuth, OAuthAuthorizationData,
    OAuthError, RedirectUriQueryParseError, UrlOrQuery,
};
use crate::{
    Client, Error, SessionChange,
    authentication::oauth::{
        AuthorizationValidationData, ClientRegistrationData, OAuthAuthorizationCodeError,
        error::{AuthorizationCodeErrorResponseType, OAuthClientRegistrationError},
    },
    test_utils::{
        client::{
            MockClientBuilder, mock_prev_session_tokens_with_refresh,
            mock_session_tokens_with_refresh,
            oauth::{mock_client_id, mock_client_metadata, mock_redirect_uri, mock_session},
        },
        mocks::MatrixMockServer,
    },
};

const REDIRECT_URI_STRING: &str = "http://127.0.0.1:6778/oauth/callback";

async fn mock_environment() -> anyhow::Result<(OAuth, MatrixMockServer, Url, ClientRegistrationData)>
{
    let server = MatrixMockServer::new().await;
    server.mock_who_am_i().ok().named("whoami").mount().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).named("server_metadata").mount().await;
    oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;
    oauth_server.mock_token().ok().mount().await;

    let client = server.client_builder().unlogged().build().await;
    let client_metadata = mock_client_metadata();

    Ok((client.oauth(), server, mock_redirect_uri(), client_metadata.into()))
}

/// Check the URL in the given authorization data.
async fn check_authorization_url(
    authorization_data: &OAuthAuthorizationData,
    oauth: &OAuth,
    server_uri: &str,
    device_id: Option<&DeviceId>,
    expected_prompt: Option<&str>,
    expected_login_hint: Option<&str>,
    additional_scopes: Option<Vec<Scope>>,
) {
    tracing::debug!("authorization data URL = {}", authorization_data.url);

    let data = oauth.data().unwrap();
    let authorization_data_guard = data.authorization_data.lock().await;
    let validation_data =
        authorization_data_guard.get(&authorization_data.state).expect("missing validation data");

    let mut num_expected =
        7 + expected_prompt.is_some() as i8 + expected_login_hint.is_some() as i8;
    let mut code_challenge = None;
    let mut prompt = None;
    let mut login_hint = None;

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
                assert_eq!(val, validation_data.redirect_uri.as_str());
                num_expected -= 1;
            }
            "scope" => {
                let actual_scopes: Vec<String> = val.split(' ').map(String::from).collect();

                assert!(actual_scopes.len() >= 2, "Expected at least two scopes");

                assert!(
                    actual_scopes
                        .contains(&"urn:matrix:org.matrix.msc2967.client:api:*".to_owned()),
                    "Expected Matrix API scope not found in scopes"
                );

                // Only check the device ID if we know it. If it's generated randomly we don't
                // know it.
                if let Some(device_id) = device_id {
                    let device_id_scope =
                        format!("urn:matrix:org.matrix.msc2967.client:device:{device_id}");
                    assert!(
                        actual_scopes.contains(&device_id_scope),
                        "Expected device ID scope not found in scopes"
                    )
                } else {
                    assert!(
                        actual_scopes
                            .iter()
                            .any(|s| s.starts_with("urn:matrix:org.matrix.msc2967.client:device:")),
                        "Expected device ID scope not found in scopes"
                    );
                }

                if let Some(additional_scopes) = &additional_scopes {
                    // Check if the additional scopes are present in the actual scopes.
                    let expected_len = 2 + additional_scopes.len();
                    assert_eq!(actual_scopes.len(), expected_len, "Expected {expected_len} scopes",);

                    for scope in additional_scopes {
                        assert!(
                            actual_scopes.contains(scope),
                            "Expected additional scope not found in scopes: {scope:?}",
                        );
                    }
                }

                num_expected -= 1;
            }
            "state" => {
                num_expected -= 1;
                assert_eq!(val, authorization_data.state.secret().as_str());
            }
            "code_challenge" => {
                code_challenge = Some(val);
                num_expected -= 1;
            }
            "code_challenge_method" => {
                assert_eq!(val, "S256");
                num_expected -= 1;
            }
            "prompt" => {
                prompt = Some(val);
                num_expected -= 1;
            }
            "login_hint" => {
                login_hint = Some(val);
                num_expected -= 1;
            }
            _ => panic!("unexpected query parameter: {key}={val}"),
        }
    }

    assert_eq!(num_expected, 0);

    let code_challenge = code_challenge.expect("missing code_challenge");
    assert_eq!(
        code_challenge,
        PkceCodeChallenge::from_code_verifier_sha256(&validation_data.pkce_verifier).as_str()
    );

    assert_eq!(prompt.as_deref(), expected_prompt);
    assert_eq!(login_hint.as_deref(), expected_login_hint);

    assert!(authorization_data.url.as_str().starts_with(server_uri));
    assert_eq!(authorization_data.url.path(), "/oauth2/authorize");
}

#[async_test]
async fn test_high_level_login() -> anyhow::Result<()> {
    // Given a fresh environment.
    let (oauth, _server, mut redirect_uri, registration_data) = mock_environment().await.unwrap();

    assert!(oauth.client_id().is_none());

    // When getting the OIDC login URL.
    let authorization_data = oauth
        .login(redirect_uri.clone(), None, Some(registration_data), None)
        .prompt(vec![Prompt::Create])
        .build()
        .await
        .unwrap();

    // Then the client should be configured correctly.
    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));

    // When completing the login with a valid callback.
    redirect_uri.set_query(Some(&format!("code=42&state={}", authorization_data.state.secret())));

    // Then the login should succeed.
    oauth.finish_login(redirect_uri.into()).await?;

    Ok(())
}

#[async_test]
async fn test_high_level_login_cancellation() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oauth, server, mut redirect_uri, registration_data) = mock_environment().await.unwrap();

    let authorization_data = oauth
        .login(redirect_uri.clone(), None, Some(registration_data), None)
        .build()
        .await
        .unwrap();

    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));

    check_authorization_url(&authorization_data, &oauth, &server.uri(), None, None, None, None)
        .await;

    // When completing login with a cancellation callback.
    redirect_uri.set_query(Some(&format!(
        "error=access_denied&state={}",
        authorization_data.state.secret()
    )));

    let error = oauth.finish_login(redirect_uri.into()).await.unwrap_err();

    // Then a cancellation error should be thrown.
    assert_matches!(
        error,
        Error::OAuth(error) => {
            assert_matches!(*error, OAuthError::AuthorizationCode(OAuthAuthorizationCodeError::Cancelled));
        }
    );

    Ok(())
}

#[async_test]
async fn test_high_level_login_invalid_state() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oauth, server, mut redirect_uri, registration_data) = mock_environment().await.unwrap();

    let authorization_data = oauth
        .login(redirect_uri.clone(), None, Some(registration_data), None)
        .build()
        .await
        .unwrap();

    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));

    check_authorization_url(&authorization_data, &oauth, &server.uri(), None, None, None, None)
        .await;

    // When completing login with an old/tampered state.
    redirect_uri.set_query(Some("code=42&state=imposter_alert"));

    let error = oauth.finish_login(redirect_uri.into()).await.unwrap_err();

    // Then the login should fail by flagging the invalid state.
    assert_matches!(
        error,
        Error::OAuth(error) => {
            assert_matches!(*error, OAuthError::AuthorizationCode(OAuthAuthorizationCodeError::InvalidState));
        }
    );

    Ok(())
}

#[async_test]
async fn test_login_url() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let server_uri = server.uri();

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(4).mount().await;

    let client = server.client_builder().registered_with_oauth().build().await;
    let oauth = client.oauth();

    let device_id = owned_device_id!("D3V1C31D"); // yo this is 1999 speaking

    let redirect_uri_str = REDIRECT_URI_STRING;
    let redirect_uri = Url::parse(redirect_uri_str)?;

    let additional_scopes =
        vec![Scope::new("urn:test:scope1".to_owned()), Scope::new("urn:test:scope2".to_owned())];

    // No extra parameters.
    let authorization_data =
        oauth.login(redirect_uri.clone(), Some(device_id.clone()), None, None).build().await?;
    check_authorization_url(
        &authorization_data,
        &oauth,
        &server_uri,
        Some(&device_id),
        None,
        None,
        None,
    )
    .await;

    // With prompt parameter.
    let authorization_data = oauth
        .login(redirect_uri.clone(), Some(device_id.clone()), None, None)
        .prompt(vec![Prompt::Create])
        .build()
        .await?;
    check_authorization_url(
        &authorization_data,
        &oauth,
        &server_uri,
        Some(&device_id),
        Some("create"),
        None,
        None,
    )
    .await;

    // With user_id_hint parameter.
    let authorization_data = oauth
        .login(redirect_uri.clone(), Some(device_id.clone()), None, None)
        .user_id_hint(user_id!("@joe:example.org"))
        .build()
        .await?;
    check_authorization_url(
        &authorization_data,
        &oauth,
        &server_uri,
        Some(&device_id),
        None,
        Some("mxid:@joe:example.org"),
        None,
    )
    .await;

    // With additional scopes.
    let authorization_data = oauth
        .login(redirect_uri.clone(), Some(device_id.clone()), None, Some(additional_scopes.clone()))
        .build()
        .await?;
    check_authorization_url(
        &authorization_data,
        &oauth,
        &server_uri,
        Some(&device_id),
        None,
        None,
        Some(additional_scopes),
    )
    .await;

    Ok(())
}

#[test]
fn test_authorization_response() -> anyhow::Result<()> {
    let uri = Url::parse("https://example.com")?;
    assert_matches!(
        AuthorizationResponse::parse_url_or_query(&uri.into()),
        Err(RedirectUriQueryParseError::MissingQuery)
    );

    let uri = Url::parse("https://example.com?code=123&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_url_or_query(&uri.into()),
        Ok(AuthorizationResponse::Success(AuthorizationCode { code, state })) => {
            assert_eq!(code, "123");
            assert_eq!(state.secret(), "456");
        }
    );

    let uri = Url::parse("https://example.com?error=invalid_scope&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_url_or_query(&uri.into()),
        Ok(AuthorizationResponse::Error(AuthorizationError { error, state })) => {
            assert_eq!(*error.error(), AuthorizationCodeErrorResponseType::InvalidScope);
            assert_eq!(error.error_description(), None);
            assert_eq!(state.secret(), "456");
        }
    );

    Ok(())
}

#[async_test]
async fn test_finish_login() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();
    let server_metadata = oauth_server.server_metadata();

    let client = server.client_builder().registered_with_oauth().build().await;
    let oauth = client.oauth();

    // If the state is missing, then any attempt to finish authorizing will fail.
    let res = oauth.finish_login(UrlOrQuery::Query("code=42&state=none".to_owned())).await;

    assert_matches!(
        res,
        Err(Error::OAuth(error)) => {
            assert_matches!(*error, OAuthError::AuthorizationCode(OAuthAuthorizationCodeError::InvalidState));
        }
    );
    assert!(client.session_tokens().is_none());
    assert!(client.session_meta().is_none());

    // Assuming a non-empty state...
    let state1 = CsrfToken::new("state1".to_owned());
    let redirect_uri = REDIRECT_URI_STRING;
    let (_pkce_code_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let auth_validation_data = AuthorizationValidationData {
        server_metadata: server_metadata.clone(),
        device_id: owned_device_id!("D3V1C31D"),
        redirect_uri: RedirectUrl::new(redirect_uri.to_owned())?,
        pkce_verifier,
    };

    {
        let data = oauth.data().context("missing data")?;
        let prev =
            data.authorization_data.lock().await.insert(state1.clone(), auth_validation_data);
        assert!(prev.is_none());
    }

    // Finishing the authorization for another state won't work.
    let res = oauth.finish_login(UrlOrQuery::Query("code=1337&state=none".to_owned())).await;

    assert_matches!(
        res,
        Err(Error::OAuth(error)) => {
            assert_matches!(*error, OAuthError::AuthorizationCode(OAuthAuthorizationCodeError::InvalidState));
        }
    );
    assert!(client.session_tokens().is_none());
    assert!(oauth.data().unwrap().authorization_data.lock().await.get(&state1).is_some());

    // Finishing the authorization for the expected state will work.
    oauth_server
        .mock_token()
        .ok_with_tokens("AT1", "RT1")
        .mock_once()
        .named("token_1")
        .mount()
        .await;
    server
        .mock_who_am_i()
        .expect_access_token("AT1")
        .ok()
        .mock_once()
        .named("whoami_1")
        .mount()
        .await;

    oauth.finish_login(UrlOrQuery::Query(format!("code=42&state={}", state1.secret()))).await?;

    let session_tokens = client.session_tokens().unwrap();
    assert_eq!(session_tokens.access_token, "AT1");
    assert_eq!(session_tokens.refresh_token.as_deref(), Some("RT1"));
    assert!(client.session_meta().is_some());
    assert!(oauth.data().unwrap().authorization_data.lock().await.get(&state1).is_none());

    // Try to log in again, with the same session.
    let state2 = CsrfToken::new("state2".to_owned());
    let redirect_uri = REDIRECT_URI_STRING;
    let (_pkce_code_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let auth_validation_data = AuthorizationValidationData {
        server_metadata: server_metadata.clone(),
        device_id: owned_device_id!("D3V1C31D"),
        redirect_uri: RedirectUrl::new(redirect_uri.to_owned())?,
        pkce_verifier,
    };

    {
        let data = oauth.data().context("missing data")?;
        let prev =
            data.authorization_data.lock().await.insert(state2.clone(), auth_validation_data);
        assert!(prev.is_none());
    }

    // Finishing the login with the same session will work again.
    oauth_server
        .mock_token()
        .ok_with_tokens("AT2", "RT2")
        .mock_once()
        .named("token_2")
        .mount()
        .await;
    server
        .mock_who_am_i()
        .expect_access_token("AT2")
        .ok()
        .mock_once()
        .named("whoami_2")
        .mount()
        .await;

    oauth.finish_login(UrlOrQuery::Query(format!("code=42&state={}", state2.secret()))).await?;

    let session_tokens = client.session_tokens().unwrap();
    assert_eq!(session_tokens.access_token, "AT2");
    assert_eq!(session_tokens.refresh_token.as_deref(), Some("RT2"));
    assert!(client.session_meta().is_some());
    assert!(oauth.data().unwrap().authorization_data.lock().await.get(&state2).is_none());

    // Try to log in again, with a different session
    let wrong_device_id = device_id!("WR0NG");
    let state3 = CsrfToken::new("state3".to_owned());
    let redirect_uri = REDIRECT_URI_STRING;
    let (_pkce_code_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let auth_validation_data = AuthorizationValidationData {
        server_metadata,
        device_id: wrong_device_id.to_owned(),
        redirect_uri: RedirectUrl::new(redirect_uri.to_owned())?,
        pkce_verifier,
    };

    {
        let data = oauth.data().context("missing data")?;
        let prev =
            data.authorization_data.lock().await.insert(state3.clone(), auth_validation_data);
        assert!(prev.is_none());
    }

    // Finishing the login with a different session will error.
    oauth_server
        .mock_token()
        .ok_with_tokens("AT3", "RT3")
        .mock_once()
        .named("token_3")
        .mount()
        .await;
    server
        .mock_who_am_i()
        .expect_access_token("AT3")
        .ok_with_device_id(wrong_device_id)
        .mock_once()
        .named("whoami_3")
        .mount()
        .await;

    let res =
        oauth.finish_login(UrlOrQuery::Query(format!("code=42&state={}", state3.secret()))).await;

    assert_matches!(
        res,
        Err(Error::OAuth(error)) => {
            assert_matches!(*error, OAuthError::SessionMismatch);
        }
    );
    assert!(oauth.data().unwrap().authorization_data.lock().await.get(&state3).is_none());

    Ok(())
}

#[async_test]
async fn test_oauth_session() -> anyhow::Result<()> {
    let client = MockClientBuilder::new(None).unlogged().build().await;
    let oauth = client.oauth();

    let tokens = mock_session_tokens_with_refresh();
    let session = mock_session(tokens.clone());
    oauth.restore_session(session.clone(), RoomLoadSettings::default()).await?;

    // Test a few extra getters.
    assert_eq!(client.session_tokens().unwrap(), tokens);

    let user_session = oauth.user_session().unwrap();
    assert_eq!(user_session.meta, session.user.meta);
    assert_eq!(user_session.tokens, tokens);

    let full_session = oauth.full_session().unwrap();

    assert_eq!(full_session.client_id.as_str(), "test_client_id");
    assert_eq!(full_session.user.meta, session.user.meta);
    assert_eq!(full_session.user.tokens, tokens);

    Ok(())
}

#[async_test]
async fn test_insecure_clients() -> anyhow::Result<()> {
    let server = MatrixMockServer::new().await;
    let server_url = server.uri();

    server.mock_well_known().ok().expect(1..).named("well_known").mount().await;
    server.mock_versions().ok().expect(1..).named("versions").mount().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(2..).named("server_metadata").mount().await;
    oauth_server.mock_token().ok().expect(2).named("token").mount().await;

    let prev_tokens = mock_prev_session_tokens_with_refresh();
    let next_tokens = mock_session_tokens_with_refresh();

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
        let oauth = client.oauth();

        // Restore the previous session so we have an existing set of refresh tokens.
        oauth
            .restore_session(mock_session(prev_tokens.clone()), RoomLoadSettings::default())
            .await?;

        let mut session_changes = client.subscribe_to_session_changes();

        // A refresh in insecure mode should work Just Fine.
        oauth.refresh_access_token().await?;

        assert_eq!(client.session_tokens().unwrap(), next_tokens);

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

    Ok(())
}

#[async_test]
async fn test_register_client() {
    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();
    let client = server.client_builder().unlogged().build().await;
    let oauth = client.oauth();
    let client_metadata = mock_client_metadata();

    // Server doesn't support registration, it fails.
    oauth_server
        .mock_server_metadata()
        .ok_without_registration()
        .expect(1)
        .named("metadata_without_registration")
        .mount()
        .await;

    let result = oauth.register_client(&client_metadata).await;
    assert_matches!(
        result,
        Err(OAuthError::ClientRegistration(OAuthClientRegistrationError::NotSupported))
    );

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

    let response = oauth.register_client(&client_metadata).await.unwrap();
    assert_eq!(response.client_id.as_str(), "test_client_id");

    let auth_data = oauth.data().unwrap();
    // There is a difference of ending slash between the strings so we parse them
    // with `Url` which will normalize that.
    assert_eq!(auth_data.client_id, response.client_id);
}

#[async_test]
async fn test_management_url_cache() {
    let server = MatrixMockServer::new().await;

    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).mount().await;

    let client = server.client_builder().logged_in_with_oauth().build().await;
    let oauth = client.oauth();

    // The cache should not contain the entry.
    assert!(!client.inner.caches.server_metadata.lock().await.contains("SERVER_METADATA"));

    let management_url = oauth
        .account_management_url()
        .await
        .expect("We should be able to fetch the account management url");

    assert!(management_url.is_some());

    // Check that the server metadata has been inserted into the cache.
    assert!(client.inner.caches.server_metadata.lock().await.contains("SERVER_METADATA"));

    // Another call doesn't make another request for the metadata.
    let management_url = oauth
        .account_management_url()
        .await
        .expect("We should be able to fetch the account management url");

    assert!(management_url.is_some());
}

#[async_test]
async fn test_server_metadata() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;
    let oauth = client.oauth();

    // The endpoint is not mocked so it is not supported.
    let error = oauth.server_metadata().await.unwrap_err();
    assert!(error.is_not_supported());

    // Mock the `GET /auth_metadata` endpoint.
    let oauth_server = server.oauth();
    oauth_server.mock_server_metadata().ok().expect(1).named("auth_metadata").mount().await;

    oauth.server_metadata().await.unwrap();
}

#[async_test]
async fn test_client_registration_data() {
    let server = MatrixMockServer::new().await;
    let oauth_server = server.oauth();
    let server_metadata = oauth_server.server_metadata();

    // Without registration we get an error.
    let client = server.client_builder().unlogged().build().await;
    let oauth = client.oauth();
    let res = oauth.use_registration_data(&server_metadata, None).await;
    assert_matches!(res, Err(OAuthError::NotRegistered));
    assert_eq!(oauth.client_id(), None);

    // With a static registration.
    let registration_data = ClientRegistrationData {
        metadata: mock_client_metadata(),
        static_registrations: Some([(server_metadata.issuer.clone(), mock_client_id())].into()),
    };
    oauth.use_registration_data(&server_metadata, Some(&registration_data)).await.unwrap();
    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));

    // If we call it again, it's a noop.
    let registration_data = ClientRegistrationData {
        metadata: mock_client_metadata(),
        static_registrations: Some(
            [(server_metadata.issuer.clone(), ClientId::new("other_client_id".to_owned()))].into(),
        ),
    };
    oauth.use_registration_data(&server_metadata, Some(&registration_data)).await.unwrap();
    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));

    // With metadata we register a new client ID.
    let client_metadata = mock_client_metadata();
    let client = server.client_builder().unlogged().build().await;
    let oauth = client.oauth();

    oauth_server
        .mock_registration()
        .ok()
        .mock_once()
        .named("registration_with_metadata")
        .mount()
        .await;

    oauth.use_registration_data(&server_metadata, Some(&client_metadata.into())).await.unwrap();
    assert_eq!(oauth.client_id().map(|id| id.as_str()), Some("test_client_id"));
}
