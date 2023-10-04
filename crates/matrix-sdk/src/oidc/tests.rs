use std::sync::Arc;

use anyhow::Context as _;
use assert_matches::assert_matches;
use mas_oidc_client::{
    requests::authorization_code::AuthorizationValidationData,
    types::{
        client_credentials::ClientCredentials,
        errors::ClientErrorCode,
        iana::oauth::OAuthClientAuthenticationMethod,
        registration::{ClientMetadata, VerifiedClientMetadata},
    },
};
use matrix_sdk_base::SessionMeta;
#[cfg(test)]
use matrix_sdk_test::async_test;
use matrix_sdk_test::test_json;
use ruma::{
    api::client::discovery::discover_homeserver::AuthenticationServerInfo, owned_user_id,
    ServerName,
};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{
    backend::mock::{MockImpl, AUTHORIZATION_URL, ISSUER_URL},
    AuthorizationCode, AuthorizationError, AuthorizationResponse, Oidc,
    OidcAccountManagementAction, OidcError, OidcSession, OidcSessionTokens,
    RedirectUriQueryParseError, UserSession,
};
use crate::{test_utils::test_client_builder, Client};

const CLIENT_ID: &str = "test_client_id";

pub fn mock_registered_client_data() -> (ClientCredentials, VerifiedClientMetadata) {
    (
        ClientCredentials::None { client_id: CLIENT_ID.to_owned() },
        ClientMetadata {
            redirect_uris: Some(vec![]), // empty vector is ok lol
            token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
            ..ClientMetadata::default()
        }
        .validate()
        .expect("validate client metadata"),
    )
}

pub fn mock_session(tokens: OidcSessionTokens) -> OidcSession {
    let (credentials, metadata) = mock_registered_client_data();
    OidcSession {
        credentials,
        metadata,
        user: UserSession {
            meta: SessionMeta {
                user_id: ruma::user_id!("@u:e.uk").to_owned(),
                device_id: ruma::device_id!("XYZ").to_owned(),
            },
            tokens,
            issuer_info: AuthenticationServerInfo::new(ISSUER_URL.to_owned(), None),
        },
    }
}

#[async_test]
async fn test_account_management_url() {
    let builder = test_client_builder(Some("https://example.com".to_owned()));
    let client = builder.build().await.unwrap();

    client
        .restore_session(OidcSession {
            credentials: ClientCredentials::None { client_id: "client_id".to_owned() },

            metadata: ClientMetadata {
                redirect_uris: Some(vec![Url::parse("https://example.com/login").unwrap()]),
                ..Default::default()
            }
            .validate()
            .unwrap(),

            user: UserSession {
                meta: SessionMeta {
                    user_id: owned_user_id!("@user:example.com"),
                    device_id: "device_id".into(),
                },
                tokens: OidcSessionTokens {
                    access_token: "access_token".to_owned(),
                    refresh_token: None,
                    latest_id_token: None,
                },
                issuer_info: AuthenticationServerInfo::new(
                    "https://example.com".to_owned(),
                    Some("https://example.com/account".to_owned()),
                ),
            },
        })
        .await
        .unwrap();

    assert_eq!(
        client.oidc().account_management_url(None).unwrap(),
        Some(Url::parse("https://example.com/account").unwrap())
    );

    assert_eq!(
        client.oidc().account_management_url(Some(OidcAccountManagementAction::Profile)).unwrap(),
        Some(Url::parse("https://example.com/account?action=profile").unwrap())
    );

    assert_eq!(
        client
            .oidc()
            .account_management_url(Some(OidcAccountManagementAction::SessionsList))
            .unwrap(),
        Some(Url::parse("https://example.com/account?action=sessions_list").unwrap())
    );

    assert_eq!(
        client
            .oidc()
            .account_management_url(Some(OidcAccountManagementAction::SessionView {
                device_id: "my_phone".into()
            }))
            .unwrap(),
        Some(
            Url::parse("https://example.com/account?action=session_view&device_id=my_phone")
                .unwrap()
        )
    );

    assert_eq!(
        client
            .oidc()
            .account_management_url(Some(OidcAccountManagementAction::SessionEnd {
                device_id: "my_old_phone".into()
            }))
            .unwrap(),
        Some(
            Url::parse("https://example.com/account?action=session_end&device_id=my_old_phone")
                .unwrap()
        )
    );
}

#[async_test]
async fn test_login() -> anyhow::Result<()> {
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await?;

    let device_id = "D3V1C31D".to_owned(); // yo this is 1999 speaking

    let oidc = Oidc { client: client.clone(), backend: Arc::new(MockImpl::new()) };

    let issuer_info = AuthenticationServerInfo::new(ISSUER_URL.to_owned(), None);
    let (client_credentials, client_metadata) = mock_registered_client_data();
    oidc.restore_registered_client(issuer_info, client_metadata, client_credentials);

    let redirect_uri_str = "http://matrix.example.com/oidc/callback";
    let redirect_uri = Url::parse(redirect_uri_str)?;
    let mut authorization_data = oidc.login(redirect_uri, Some(device_id.clone()))?.build().await?;

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
                assert_eq!(val, CLIENT_ID);
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

    authorization_data.url.set_query(None);
    assert_eq!(authorization_data.url, Url::parse(AUTHORIZATION_URL).unwrap(),);

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
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await?;

    let session_tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3$h".to_owned()),
        latest_id_token: None,
    };
    let oidc = Oidc {
        client: client.clone(),
        backend: Arc::new(MockImpl::new().next_session_tokens(session_tokens.clone())),
    };

    let issuer_info = AuthenticationServerInfo::new(ISSUER_URL.to_owned(), None);
    let (client_credentials, client_metadata) = mock_registered_client_data();
    oidc.restore_registered_client(issuer_info, client_metadata, client_credentials);

    // If the state is missing, then any attempt to finish authorizing will fail.
    let res = oidc
        .finish_authorization(AuthorizationCode { code: "42".to_owned(), state: "none".to_owned() })
        .await;

    assert_matches!(res, Err(OidcError::InvalidState));
    assert!(oidc.session_tokens().is_none());

    // Assuming a non-empty state "123"...
    let state = "state".to_owned();
    let redirect_uri = "http://matrix.example.com/oidc/callback";
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

    assert_eq!(oidc.session_tokens(), Some(session_tokens));
    assert!(oidc.data().unwrap().authorization_data.lock().await.get(&state).is_none());

    Ok(())
}

#[async_test]
async fn test_insecure_clients() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let server_url = server.uri();

    Mock::given(method("GET"))
        .and(path("/.well-known/matrix/client"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
            "application/json",
        ))
        .mount(&server)
        .await;

    let prev_tokens = OidcSessionTokens {
        access_token: "prev-access-token".to_owned(),
        refresh_token: Some("prev-refresh-token".to_owned()),
        latest_id_token: None,
    };

    let next_tokens = OidcSessionTokens {
        access_token: "next-access-token".to_owned(),
        refresh_token: Some("next-refresh-token".to_owned()),
        latest_id_token: None,
    };

    for client in [
        // Create an insecure client with the homeserver_url method.
        Client::builder().homeserver_url("http://example.org").build().await?,
        // Create an insecure client with the insecure_server_name_no_tls method.
        Client::builder()
            .insecure_server_name_no_tls(&ServerName::parse(
                server_url.strip_prefix("http://").unwrap(),
            )?)
            .build()
            .await?,
    ] {
        let backend = Arc::new(
            MockImpl::new()
                .mark_insecure()
                .next_session_tokens(next_tokens.clone())
                .expected_refresh_token(prev_tokens.refresh_token.as_ref().unwrap().clone()),
        );
        let oidc = Oidc { client: client.clone(), backend: backend.clone() };

        // Restore the previous session so we have an existing set of refresh tokens.
        oidc.restore_session(mock_session(prev_tokens.clone())).await?;

        // A refresh in insecure mode should work Just Fine.
        oidc.refresh_access_token().await?;

        // There should have been exactly one refresh.
        assert_eq!(*backend.num_refreshes.lock().unwrap(), 1);
    }

    Ok(())
}
