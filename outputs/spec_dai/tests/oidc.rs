//! The OpenID Connect login flow against a mock identity provider.

mod common;

use axum::extract::{Form, State};
use common::*;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserEmail, IssuerUrl, JsonWebKeyId, Nonce,
    PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEY_PEM: &str = include_str!("fixtures/test_rsa_key.pem");

struct Idp {
    issuer: String,
    /// code -> (nonce, email)
    codes: Mutex<HashMap<String, (String, String)>>,
    last_token_form: Mutex<HashMap<String, String>>,
}

fn signing_key() -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(KEY_PEM, Some(JsonWebKeyId::new("k1".into()))).unwrap()
}

async fn start_idp() -> (String, Arc<Idp>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let idp = Arc::new(Idp {
        issuer: issuer.clone(),
        codes: Mutex::new(HashMap::new()),
        last_token_form: Mutex::new(HashMap::new()),
    });
    let app = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(|State(idp): State<Arc<Idp>>| async move {
                axum::Json(json!({
                    "issuer": idp.issuer,
                    "authorization_endpoint": format!("{}/authorize", idp.issuer),
                    "token_endpoint": format!("{}/token", idp.issuer),
                    "jwks_uri": format!("{}/jwks", idp.issuer),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["RS256"],
                }))
            }),
        )
        .route(
            "/jwks",
            axum::routing::get(|| async {
                axum::Json(json!({"keys": [signing_key().as_verification_key()]}))
            }),
        )
        .route(
            "/token",
            axum::routing::post(
                |State(idp): State<Arc<Idp>>, Form(form): Form<HashMap<String, String>>| async move {
                    *idp.last_token_form.lock().unwrap() = form.clone();
                    let code = form.get("code").cloned().unwrap_or_default();
                    let Some((nonce, email)) = idp.codes.lock().unwrap().remove(&code) else {
                        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"})));
                    };
                    let now = chrono::Utc::now();
                    let claims = CoreIdTokenClaims::new(
                        IssuerUrl::new(idp.issuer.clone()).unwrap(),
                        vec![Audience::new("moxy".into())],
                        now + chrono::Duration::minutes(5),
                        now,
                        StandardClaims::new(SubjectIdentifier::new(format!("sub-{email}")))
                            .set_email(Some(EndUserEmail::new(email.clone())))
                            .set_email_verified(Some(true)),
                        EmptyAdditionalClaims {},
                    )
                    .set_nonce(Some(Nonce::new(nonce)));
                    let token = CoreIdToken::new(
                        claims,
                        &signing_key(),
                        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
                        None,
                        None,
                    )
                    .unwrap();
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "access_token": "at",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "id_token": serde_json::to_value(&token).unwrap(),
                        })),
                    )
                },
            ),
        )
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (issuer, idp)
}

fn params(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Run the browser side of the flow; returns the Set-Cookie value and redirect target.
async fn sign_in(m: &Moxy, idp: &Idp, email: &str, return_to: &str) -> (String, String) {
    let c = client();
    let r = c
        .get(m.url(&format!("/auth/login?return_to={return_to}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    assert!(loc.starts_with(&format!("{}/authorize?", idp.issuer)), "{loc}");
    let p = params(&loc);
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["client_id"], "moxy");
    assert_eq!(p["code_challenge_method"], "S256");
    assert_eq!(p["redirect_uri"], m.url("/auth/callback"));
    assert!(p["scope"].contains("openid"));
    // The IdP authenticates the user and redirects back with a code.
    let code = moxy::util::random_hex(8);
    idp.codes
        .lock()
        .unwrap()
        .insert(code.clone(), (p["nonce"].clone(), email.to_string()));
    let r = c
        .get(m.url(&format!("/auth/callback?code={code}&state={}", p["state"])))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302, "{:?}", r.text().await);
    let cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
    let target = r.headers()["location"].to_str().unwrap().to_string();
    // PKCE verifier was sent with the code exchange.
    assert!(idp.last_token_form.lock().unwrap().contains_key("code_verifier"));
    (cookie, target)
}

#[tokio::test(flavor = "multi_thread")]
async fn login_callback_session_and_roles() {
    let (issuer, idp) = start_idp().await;
    let (m, _) = Moxy::start_with_env(
        &format!(
            r#"<moxy version="1"><server port="{{PORT}}" />
  <auth issuer="{issuer}" client-id="moxy" client-secret="${{SECRET}}" session-ttl="2h">
    <admin email="boss@example.com" />
  </auth>
</moxy>"#
        ),
        &[("SECRET", "shh")],
    );
    let c = client();

    let (cookie, target) = sign_in(&m, &idp, "boss@example.com", "/admin").await;
    assert_eq!(target, "/admin");
    assert!(cookie.starts_with("moxy_session="));
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax") && cookie.contains("Path=/"));
    assert!(!cookie.contains("Secure"), "bound to 127.0.0.1");
    let session = cookie.split(';').next().unwrap().to_string();
    let me: Value = json(c.get(m.url("/api/me")).header("cookie", &session).send().await.unwrap()).await;
    assert_eq!(me["role"], "admin");
    assert_eq!(me["email"], "boss@example.com");
    assert_eq!(me["sub"], "sub-boss@example.com");
    let r = c.get(m.url("/admin")).header("cookie", &session).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let (cookie, target) = sign_in(&m, &idp, "dev@example.com", "//evil.example.com").await;
    assert_eq!(target, "/", "only same-origin relative paths are accepted");
    let session = cookie.split(';').next().unwrap().to_string();
    let me: Value = json(c.get(m.url("/api/me")).header("cookie", &session).send().await.unwrap()).await;
    assert_eq!(me["role"], "user");
    assert_eq!(c.get(m.url("/chat")).header("cookie", &session).send().await.unwrap().status(), 200);
    assert_eq!(c.get(m.url("/admin")).header("cookie", &session).send().await.unwrap().status(), 403);

    // Replaying a used or unknown state is rejected.
    let r = c.get(m.url("/auth/callback?code=x&state=unknown")).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(json(r).await["error"]["type"], "invalid_state");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_without_auth_config() {
    let (m, _) = Moxy::start(r#"<moxy version="1"><server port="{PORT}" /></moxy>"#);
    let r = client().get(m.url("/auth/login")).send().await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(json(r).await["error"]["type"], "auth_not_configured");
}
