//! Users, sessions, CSRF, API tokens, roles and tenant isolation, through the HTTP API.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbitd::api::{AppState, router};
use orbitd::auth::RequestMeta;
use orbitd::executor::Executor;
use orbitd::gateway::dns::StaticDns;
use orbitd::node::Node;
use orbitd::platform::Platform;
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "break-glass";
const PASSWORD: &str = "correct horse battery";

struct Fx {
    _dir: tempfile::TempDir,
    app: Router,
    state: AppState,
    dns: StaticDns,
}

async fn fx(admin_token: Option<&str>) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let exec = Arc::new(Executor::new(store, dir.path(), false).unwrap());
    let node = Node::new(exec);
    let dns = StaticDns::default();
    let mut state = AppState::new(node, admin_token.map(Arc::from));
    state.platform = Arc::new(Platform::new(
        Some("cloud.test".into()),
        Arc::new(dns.clone()),
    ));
    let app = router(state.clone());
    Fx {
        _dir: dir,
        app,
        state,
        dns,
    }
}

#[derive(Default, Clone)]
struct Who {
    bearer: Option<String>,
    cookie: Option<String>,
    csrf: Option<String>,
    origin: Option<String>,
    https: bool,
}

impl Who {
    fn bearer(t: &str) -> Self {
        Self {
            bearer: Some(t.into()),
            ..Self::default()
        }
    }
}

struct Resp {
    status: StatusCode,
    json: Value,
    set_cookie: Option<String>,
}

async fn call(app: &Router, who: &Who, method: &str, uri: &str, body: Option<Value>) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "console.cloud.test");
    if let Some(t) = &who.bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(c) = &who.cookie {
        b = b.header("cookie", c);
    }
    if let Some(c) = &who.csrf {
        b = b.header("x-csrf-token", c);
    }
    if let Some(o) = &who.origin {
        b = b.header("origin", o);
    }
    let mut req = b
        .header("content-type", "application/json")
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .unwrap();
    req.extensions_mut().insert(RequestMeta {
        ip: Some("198.51.100.1".parse().unwrap()),
        https: who.https,
    });
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        set_cookie,
    }
}

/// `name=value` of a Set-Cookie header.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

/// Creates the first owner through setup and returns a signed-in `Who`.
async fn setup(f: &Fx, email: &str) -> Who {
    *f.state.platform.setup_token.lock().unwrap() = Some("setup-secret".into());
    let r = call(
        &f.app,
        &Who::default(),
        "POST",
        "/v1/setup",
        Some(json!({"token": "setup-secret", "email": email, "display_name": "Owner", "password": PASSWORD})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    Who {
        cookie: Some(cookie_pair(r.set_cookie.as_ref().unwrap())),
        csrf: r.json["csrf_token"].as_str().map(str::to_owned),
        ..Who::default()
    }
}

async fn login(f: &Fx, email: &str, password: &str) -> Resp {
    call(
        &f.app,
        &Who::default(),
        "POST",
        "/v1/auth/login",
        Some(json!({"email": email, "password": password})),
    )
    .await
}

fn session_of(r: &Resp) -> Who {
    Who {
        cookie: Some(cookie_pair(r.set_cookie.as_ref().unwrap())),
        csrf: r.json["csrf_token"].as_str().map(str::to_owned),
        ..Who::default()
    }
}

fn wasm() -> Vec<u8> {
    wat::parse_str(r#"(module (func (export "_start")))"#).unwrap()
}

async fn deploy(app: &Router, who: &Who, project: &str, name: &str, spec: &str) -> StatusCode {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{project}/apps/{name}/deployments"))
        .header("x-orbit-spec", spec);
    if let Some(t) = &who.bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(c) = &who.cookie {
        b = b.header("cookie", c);
    }
    if let Some(c) = &who.csrf {
        b = b.header("x-csrf-token", c);
    }
    app.clone()
        .oneshot(b.body(Body::from(wasm())).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn first_boot_setup_then_login_and_session() {
    let f = fx(None).await;
    // before setup an open node answers anyone (development)
    assert_eq!(
        call(&f.app, &Who::default(), "GET", "/v1/apps", None)
            .await
            .status,
        StatusCode::OK
    );
    // wrong token, then the right one
    let bad = call(
        &f.app,
        &Who::default(),
        "POST",
        "/v1/setup",
        Some(json!({"token": "nope", "email": "o@x.test", "password": PASSWORD})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    let owner = setup(&f, "Owner@X.test").await;
    // setup is one-shot
    *f.state.platform.setup_token.lock().unwrap() = Some("setup-secret".into());
    let again = call(
        &f.app,
        &Who::default(),
        "POST",
        "/v1/setup",
        Some(json!({"token": "setup-secret", "email": "b@x.test", "password": PASSWORD})),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    // the node is closed now
    assert_eq!(
        call(&f.app, &Who::default(), "GET", "/v1/apps", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // the setup session works
    let me = call(&f.app, &owner, "GET", "/v1/me", None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["user"]["email"], "owner@x.test");
    assert_eq!(me.json["memberships"][0]["role"], "owner");
    // login: wrong password, then right; cookie flags
    assert_eq!(
        login(&f, "owner@x.test", "wrong password!").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&f, "nobody@x.test", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    let ok = login(&f, "OWNER@x.test", PASSWORD).await;
    assert_eq!(ok.status, StatusCode::OK);
    let c = ok.set_cookie.clone().unwrap();
    assert!(c.starts_with("orbit_session="), "{c}");
    assert!(
        c.contains("HttpOnly") && c.contains("SameSite=Lax") && !c.contains("Secure"),
        "{c}"
    );
    // the database holds only a hash of the session id
    let value = c
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("orbit_session=");
    let store = f.state.store();
    assert!(
        store
            .session(&oc_store::sha256_hex(value), 1000)
            .unwrap()
            .is_some()
    );
    assert!(
        store.session(value, 1000).unwrap().is_none(),
        "the raw id is not a key"
    );
    // logout ends the session
    let who = session_of(&ok);
    assert_eq!(
        call(&f.app, &who, "POST", "/v1/auth/logout", None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&f.app, &who, "GET", "/v1/me", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn cookie_is_secure_on_https() {
    let f = fx(Some(ADMIN)).await;
    setup(&f, "o@x.test").await;
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/auth/login")
        .header("host", "console.cloud.test")
        .body(Body::from(
            json!({"email": "o@x.test", "password": PASSWORD}).to_string(),
        ))
        .unwrap();
    req.extensions_mut().insert(RequestMeta {
        ip: None,
        https: true,
    });
    let r = f.app.clone().oneshot(req).await.unwrap();
    let c = r.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(c.starts_with("__Host-orbit_session="), "{c}");
    assert!(
        c.contains("Secure")
            && c.contains("HttpOnly")
            && c.contains("SameSite=Lax")
            && c.contains("Path=/")
    );
}

#[tokio::test]
async fn csrf_is_required_for_cookie_mutations_only() {
    let f = fx(Some(ADMIN)).await;
    let owner = setup(&f, "o@x.test").await;
    let new_project = Some(json!({"org": "default", "name": "shop"}));
    // GETs need no token
    assert_eq!(
        call(&f.app, &owner, "GET", "/v1/projects", None)
            .await
            .status,
        StatusCode::OK
    );
    // a cookie-only POST is refused
    let mut no_csrf = owner.clone();
    no_csrf.csrf = None;
    assert_eq!(
        call(
            &f.app,
            &no_csrf,
            "POST",
            "/v1/projects",
            new_project.clone()
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // a wrong token is refused
    let mut wrong = owner.clone();
    wrong.csrf = Some("0".repeat(64));
    assert_eq!(
        call(&f.app, &wrong, "POST", "/v1/projects", new_project.clone())
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // a cross-origin POST with the right token is still refused
    let mut cross = owner.clone();
    cross.origin = Some("https://evil.example".into());
    assert_eq!(
        call(&f.app, &cross, "POST", "/v1/projects", new_project.clone())
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // same-origin with the token works
    let mut same = owner.clone();
    same.origin = Some("https://console.cloud.test".into());
    assert_eq!(
        call(&f.app, &same, "POST", "/v1/projects", new_project)
            .await
            .status,
        StatusCode::CREATED
    );
    // bearer tokens are not cookie-authenticated: no CSRF header needed
    assert_eq!(
        call(
            &f.app,
            &Who::bearer(ADMIN),
            "POST",
            "/v1/projects",
            Some(json!({"org": "default", "name": "blog"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn failed_logins_are_rate_limited() {
    let f = fx(Some(ADMIN)).await;
    setup(&f, "o@x.test").await;
    for _ in 0..5 {
        assert_eq!(
            login(&f, "o@x.test", "wrong wrong wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let blocked = login(&f, "o@x.test", PASSWORD).await;
    assert_eq!(
        blocked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "even the right password waits"
    );
}

/// Owner creates org `acme` (project `shop`) and a user in it.
async fn tenant(f: &Fx) -> (Who, Who) {
    let admin = Who::bearer(ADMIN);
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/orgs",
            Some(json!({"name": "acme"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/projects",
            Some(json!({"org": "acme", "name": "shop"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    let r = call(
        &f.app,
        &admin,
        "POST",
        "/v1/users",
        Some(json!({"email": "dev@acme.test", "password": PASSWORD, "org": "acme", "role": "developer"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let dev = session_of(&login(f, "dev@acme.test", PASSWORD).await);
    (admin, dev)
}

#[tokio::test]
async fn roles_gate_actions_and_hide_other_tenants() {
    let f = fx(Some(ADMIN)).await;
    setup(&f, "root@x.test").await;
    let (admin, dev) = tenant(&f).await;
    // developer: can deploy and read in their project
    assert_eq!(
        deploy(&f.app, &dev, "shop", "web", "{}").await,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/projects/shop/apps", None)
            .await
            .json[0]["name"],
        "web"
    );
    // ... but not manage (domains), and not own (delete project)
    assert_eq!(
        call(
            &f.app,
            &dev,
            "POST",
            "/v1/projects/shop/domains",
            Some(json!({"host": "shop.kunde.test"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.app, &dev, "DELETE", "/v1/projects/shop", None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // the instance needs an administrator
    assert_eq!(
        call(
            &f.app,
            &dev,
            "POST",
            "/v1/orgs",
            Some(json!({"name": "mine"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/users", None).await.status,
        StatusCode::FORBIDDEN
    );
    // another tenant's project does not exist for the developer
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/projects/default/apps", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/projects/nope/apps", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        deploy(&f.app, &dev, "default", "x", "{}").await,
        StatusCode::NOT_FOUND
    );
    // legacy /v1/apps is the default project: invisible to this developer
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/apps", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/apps?project=shop", None)
            .await
            .status,
        StatusCode::OK
    );
    let list = call(&f.app, &dev, "GET", "/v1/projects", None).await.json;
    assert_eq!(
        list.as_array().unwrap().len(),
        1,
        "only their project is listed: {list}"
    );
    // a run of another project cannot be fetched by id either
    assert_eq!(
        deploy(&f.app, &admin, "default", "hello", "{}").await,
        StatusCode::CREATED
    );
    let run = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/default/apps/hello/runs",
        None,
    )
    .await;
    assert_eq!(run.status, StatusCode::OK, "{:?}", run.json);
    let id = run.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(&f.app, &dev, "GET", &format!("/v1/runs/{id}"), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.app, &dev, "GET", &format!("/v1/runs/{id}/trace"), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.app, &admin, "GET", &format!("/v1/runs/{id}"), None)
            .await
            .status,
        StatusCode::OK
    );
    // apps in different projects keep apart even with the same name
    assert_eq!(
        deploy(&f.app, &dev, "shop", "hello", "{}").await,
        StatusCode::CREATED
    );
    let mine = call(&f.app, &dev, "GET", "/v1/projects/shop/apps/hello", None)
        .await
        .json;
    assert_eq!(mine["app"]["key"], "shop.hello");
    // node-wide listings are scoped too
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/projects/shop/runs", None)
            .await
            .json
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn viewer_reads_and_cannot_write() {
    let f = fx(Some(ADMIN)).await;
    let (admin, dev) = tenant(&f).await;
    assert_eq!(
        deploy(&f.app, &dev, "shop", "web", "{}").await,
        StatusCode::CREATED
    );
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/users",
        Some(json!({"email": "view@acme.test", "password": PASSWORD, "org": "acme", "role": "viewer"})),
    )
    .await;
    let viewer = session_of(&login(&f, "view@acme.test", PASSWORD).await);
    assert_eq!(
        call(&f.app, &viewer, "GET", "/v1/projects/shop/apps/web", None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        deploy(&f.app, &viewer, "shop", "web", "{}").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.app,
            &viewer,
            "PUT",
            "/v1/projects/shop/apps/web/kv/ns/k",
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.app,
            &viewer,
            "DELETE",
            "/v1/projects/shop/apps/web",
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn api_tokens_are_scoped_capped_and_revocable() {
    let f = fx(Some(ADMIN)).await;
    let (admin, dev) = tenant(&f).await;
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects",
        Some(json!({"org": "default", "name": "other"})),
    )
    .await;
    // the developer mints a CI token for their project
    let t = call(
        &f.app,
        &dev,
        "POST",
        "/v1/tokens",
        Some(json!({"name": "ci", "scope": {"project": "shop"}, "role": "developer", "expires_in_days": 30})),
    )
    .await;
    assert_eq!(t.status, StatusCode::CREATED, "{:?}", t.json);
    let secret = t.json["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("oc_"));
    let tok_id = t.json["id"].as_str().unwrap().to_owned();
    // only a hash is stored
    let stored = f.state.store().tokens(None).unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].prefix.len() == 10 && secret.starts_with(&stored[0].prefix));
    // the token works inside its scope ...
    let ci = Who::bearer(&secret);
    assert_eq!(
        deploy(&f.app, &ci, "shop", "web", "{}").await,
        StatusCode::CREATED
    );
    // ... not in another project, not above its role, and cannot mint tokens
    assert_eq!(
        call(&f.app, &ci, "GET", "/v1/projects/other/apps", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.app,
            &ci,
            "POST",
            "/v1/projects/shop/domains",
            Some(json!({"host": "a.kunde.test"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.app,
            &ci,
            "POST",
            "/v1/tokens",
            Some(json!({"name": "x", "scope": {"project": "shop"}, "role": "viewer"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // a token never has more power than its creator: a developer cannot mint an admin token
    assert_eq!(
        call(
            &f.app,
            &dev,
            "POST",
            "/v1/tokens",
            Some(json!({"name": "up", "scope": {"project": "shop"}, "role": "admin"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // ... nor for a project they cannot see
    assert_eq!(
        call(
            &f.app,
            &dev,
            "POST",
            "/v1/tokens",
            Some(json!({"name": "x", "scope": {"project": "other"}, "role": "viewer"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // listing shows no secrets and only the caller's tokens
    let listed = call(&f.app, &dev, "GET", "/v1/tokens", None).await.json;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(listed[0].get("token").is_none());
    // demoting the creator demotes the token
    let dev_user = f
        .state
        .store()
        .user_by_email("dev@acme.test")
        .unwrap()
        .unwrap();
    let acme = f.state.store().org("acme").unwrap();
    f.state
        .store()
        .set_member(&acme.id, &dev_user.id, oc_store::Role::Viewer)
        .unwrap();
    assert_eq!(
        deploy(&f.app, &ci, "shop", "web", "{}").await,
        StatusCode::FORBIDDEN
    );
    f.state
        .store()
        .set_member(&acme.id, &dev_user.id, oc_store::Role::Developer)
        .unwrap();
    assert_eq!(
        deploy(&f.app, &ci, "shop", "web", "{}").await,
        StatusCode::CREATED
    );
    // revoke
    assert_eq!(
        call(
            &f.app,
            &dev,
            "DELETE",
            &format!("/v1/tokens/{tok_id}"),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        deploy(&f.app, &ci, "shop", "web", "{}").await,
        StatusCode::UNAUTHORIZED
    );
    // garbage tokens
    assert_eq!(
        call(
            &f.app,
            &Who::bearer("oc_deadbeef"),
            "GET",
            "/v1/projects",
            None
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // org-scoped viewer token sees every project of the org but writes none
    let v = call(
        &f.app,
        &admin,
        "POST",
        "/v1/tokens",
        Some(json!({"name": "ro", "scope": {"org": "acme"}, "role": "viewer"})),
    )
    .await;
    let ro = Who::bearer(v.json["token"].as_str().unwrap());
    assert_eq!(
        call(&f.app, &ro, "GET", "/v1/projects/shop/apps", None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        deploy(&f.app, &ro, "shop", "web", "{}").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.app, &ro, "GET", "/v1/projects/other/apps", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn expired_tokens_and_disabled_users_are_refused() {
    let f = fx(Some(ADMIN)).await;
    let (admin, dev) = tenant(&f).await;
    let t = call(
        &f.app,
        &dev,
        "POST",
        "/v1/tokens",
        Some(json!({"name": "ci", "scope": {"project": "shop"}, "role": "viewer"})),
    )
    .await;
    let ci = Who::bearer(t.json["token"].as_str().unwrap());
    assert_eq!(
        call(&f.app, &ci, "GET", "/v1/projects/shop/apps", None)
            .await
            .status,
        StatusCode::OK
    );
    let u = f
        .state
        .store()
        .user_by_email("dev@acme.test")
        .unwrap()
        .unwrap();
    f.state.store().set_user_disabled(&u.id, true).unwrap();
    assert_eq!(
        call(&f.app, &ci, "GET", "/v1/projects/shop/apps", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/me", None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&f, "dev@acme.test", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    // the break-glass token is unaffected
    assert_eq!(
        call(&f.app, &admin, "GET", "/v1/projects", None)
            .await
            .status,
        StatusCode::OK
    );
    // an expired token
    f.state
        .store()
        .create_token(
            "old",
            "oc_old0000",
            &oc_store::sha256_hex("oc_old"),
            None,
            &oc_store::TokenScope::Org("org_default".into()),
            oc_store::Role::Viewer,
            Some(1),
        )
        .unwrap();
    assert_eq!(
        call(&f.app, &Who::bearer("oc_old"), "GET", "/v1/projects", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn members_and_the_last_owner() {
    let f = fx(Some(ADMIN)).await;
    let (admin, dev) = tenant(&f).await;
    let members = call(&f.app, &admin, "GET", "/v1/orgs/acme/members", None)
        .await
        .json;
    assert_eq!(members.as_array().unwrap().len(), 1);
    // a developer cannot add members
    assert_eq!(
        call(
            &f.app,
            &dev,
            "POST",
            "/v1/orgs/acme/members",
            Some(json!({"email": "dev@acme.test", "role": "owner"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // an admin cannot hand out ownership, an owner can
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/users",
        Some(
            json!({"email": "adm@acme.test", "password": PASSWORD, "org": "acme", "role": "admin"}),
        ),
    )
    .await;
    let adm = session_of(&login(&f, "adm@acme.test", PASSWORD).await);
    assert_eq!(
        call(
            &f.app,
            &adm,
            "POST",
            "/v1/orgs/acme/members",
            Some(json!({"email": "dev@acme.test", "role": "owner"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/orgs/acme/members",
            Some(json!({"email": "dev@acme.test", "role": "owner"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    // unknown user
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/orgs/acme/members",
            Some(json!({"email": "ghost@acme.test", "role": "viewer"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn audit_log_records_who_did_what() {
    let f = fx(Some(ADMIN)).await;
    let (admin, dev) = tenant(&f).await;
    assert_eq!(
        deploy(&f.app, &dev, "shop", "web", "{}").await,
        StatusCode::CREATED
    );
    // developers cannot read the org audit log, an admin can
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/orgs/acme/audit", None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let log = call(&f.app, &admin, "GET", "/v1/orgs/acme/audit", None).await;
    assert_eq!(log.status, StatusCode::OK);
    let entries = log.json.as_array().unwrap();
    let deploy_entry = entries
        .iter()
        .find(|e| e["action"] == "app.deploy")
        .expect("deploy is audited");
    assert_eq!(deploy_entry["actor_kind"], "user");
    assert_eq!(deploy_entry["actor_label"], "dev@acme.test");
    assert_eq!(deploy_entry["target"], "shop.web");
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "project.create" && e["actor_kind"] == "admin-token")
    );
    // newest first
    assert!(entries[0]["id"].as_i64() > entries[entries.len() - 1]["id"].as_i64());
    // the global log is for instance administrators
    assert_eq!(
        call(&f.app, &dev, "GET", "/v1/audit", None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.app, &admin, "GET", "/v1/audit", None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn legacy_and_project_paths_are_the_same_surface() {
    let f = fx(Some(ADMIN)).await;
    let admin = Who::bearer(ADMIN);
    assert_eq!(
        deploy(&f.app, &admin, "default", "hello", "{}").await,
        StatusCode::CREATED
    );
    // the milestone-1 path and the project path show the same app
    let a = call(&f.app, &admin, "GET", "/v1/apps", None).await.json;
    let b = call(&f.app, &admin, "GET", "/v1/projects/default/apps", None)
        .await
        .json;
    assert_eq!(a, b);
    assert_eq!(a[0]["name"], "hello");
    assert_eq!(a[0]["project"], "default");
    let c = call(&f.app, &admin, "GET", "/v1/apps?project=default", None)
        .await
        .json;
    assert_eq!(a, c);
    // projects are separate namespaces
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects",
        Some(json!({"org": "default", "name": "p2"})),
    )
    .await;
    assert_eq!(
        call(&f.app, &admin, "GET", "/v1/projects/p2/apps", None)
            .await
            .json,
        json!([])
    );
    // reserved and duplicate names
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/projects",
            Some(json!({"org": "default", "name": "console"}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &f.app,
            &admin,
            "POST",
            "/v1/projects",
            Some(json!({"org": "default", "name": "p2"}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    // KV, queues and logs follow the project
    let put = Request::builder()
        .method("PUT")
        .uri("/v1/projects/default/apps/hello/kv/ns/k")
        .header("authorization", format!("Bearer {ADMIN}"))
        .body(Body::from("v"))
        .unwrap();
    assert_eq!(
        f.app.clone().oneshot(put).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let kv = call(&f.app, &admin, "GET", "/v1/apps/hello/kv/ns", None)
        .await
        .json;
    assert_eq!(kv[0]["key"], "k");
    let q = Request::builder()
        .method("POST")
        .uri("/v1/projects/p2/queues/jobs/messages")
        .header("authorization", format!("Bearer {ADMIN}"))
        .body(Body::from("m"))
        .unwrap();
    assert_eq!(
        f.app.clone().oneshot(q).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        call(&f.app, &admin, "GET", "/v1/projects/p2/queues", None)
            .await
            .json[0]["queue"],
        "jobs"
    );
    assert_eq!(
        call(&f.app, &admin, "GET", "/v1/queues", None).await.json,
        json!([]),
        "default project does not see p2's queue"
    );
    // deleting a project needs its apps gone, or force
    deploy(&f.app, &admin, "p2", "web", "{}").await;
    assert_eq!(
        call(&f.app, &admin, "DELETE", "/v1/projects/p2", None)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&f.app, &admin, "DELETE", "/v1/projects/p2?force=true", None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn domains_flow_through_the_api() {
    let f = fx(Some(ADMIN)).await;
    let admin = Who::bearer(ADMIN);
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects",
        Some(json!({"org": "default", "name": "shop"})),
    )
    .await;
    let bad = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/domains",
        Some(json!({"host": "x.cloud.test"})),
    )
    .await;
    assert_eq!(
        bad.status,
        StatusCode::BAD_REQUEST,
        "under the platform domain"
    );
    let d = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/domains",
        Some(json!({"host": "Shop.Kunde.test"})),
    )
    .await;
    assert_eq!(d.status, StatusCode::CREATED, "{:?}", d.json);
    assert_eq!(d.json["host"], "shop.kunde.test");
    assert_eq!(d.json["cname"]["value"], "shop.cloud.test");
    assert_eq!(d.json["txt"]["name"], "_orbit-verify.shop.kunde.test");
    let txt = d.json["txt"]["value"].as_str().unwrap().to_owned();
    assert!(txt.starts_with("orbit-verify="));
    // not verified before the record exists
    let v = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/domains/shop.kunde.test/verify",
        None,
    )
    .await;
    assert_eq!(v.json["status"], "pending");
    assert!(v.json["last_error"].as_str().unwrap().contains("no TXT"));
    // the customer sets the record
    f.dns.set_txt("_orbit-verify.shop.kunde.test", &[&txt]);
    f.dns
        .set_addrs("shop.kunde.test", &["203.0.113.5".parse().unwrap()]);
    f.dns
        .set_addrs("shop.cloud.test", &["203.0.113.5".parse().unwrap()]);
    let v = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/domains/shop.kunde.test/verify",
        None,
    )
    .await;
    assert_eq!(v.json["status"], "verified");
    assert_eq!(v.json["cname_ok"], true);
    let list = call(&f.app, &admin, "GET", "/v1/projects/shop/domains", None)
        .await
        .json;
    assert_eq!(list[0]["status"], "verified");
    // a second project cannot take a verified host
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects",
        Some(json!({"org": "default", "name": "other"})),
    )
    .await;
    let dup = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/other/domains",
        Some(json!({"host": "shop.kunde.test"})),
    )
    .await;
    assert_eq!(dup.status, StatusCode::CONFLICT);
    assert_eq!(
        call(
            &f.app,
            &admin,
            "DELETE",
            "/v1/projects/shop/domains/shop.kunde.test",
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn http_services_get_a_port_and_the_root_route() {
    let f = fx(Some(ADMIN)).await;
    let admin = Who::bearer(ADMIN);
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects",
        Some(json!({"org": "default", "name": "shop"})),
    )
    .await;
    assert_eq!(
        deploy(
            &f.app,
            &admin,
            "shop",
            "web",
            r#"{"kind":"service","http":true,"restart":"never"}"#
        )
        .await,
        StatusCode::CREATED
    );
    assert_eq!(
        deploy(&f.app, &admin, "shop", "worker", r#"{"kind":"job"}"#).await,
        StatusCode::CREATED
    );
    let routes = call(&f.app, &admin, "GET", "/v1/projects/shop/routes", None)
        .await
        .json;
    assert_eq!(routes[0]["path_prefix"], "/");
    assert_eq!(routes[0]["app"], "web");
    let port = f
        .state
        .store()
        .app_port("shop.web")
        .unwrap()
        .expect("a port was assigned");
    assert!((20000..=29999).contains(&port));
    assert!(f.state.store().app_port("shop.worker").unwrap().is_none());
    // more routes: only to HTTP apps of the project
    let r = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/routes",
        Some(json!({"path_prefix": "/api", "app": "worker"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/routes",
        Some(json!({"path_prefix": "/api", "app": "web"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let r = call(
        &f.app,
        &admin,
        "POST",
        "/v1/projects/shop/routes",
        Some(json!({"path_prefix": "/x", "app": "web", "host": "elsewhere.test"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let w = call(&f.app, &admin, "GET", "/v1/projects/shop/apps/web", None).await;
    assert_eq!(w.json["app"]["key"], "shop.web");
}

#[tokio::test]
async fn the_last_owner_cannot_leave() {
    let f = fx(Some(ADMIN)).await;
    let (admin, _dev) = tenant(&f).await;
    // `tenant` made no owner user; the break-glass admin created the org. Make one.
    call(
        &f.app,
        &admin,
        "POST",
        "/v1/users",
        Some(
            json!({"email": "own@acme.test", "password": PASSWORD, "org": "acme", "role": "owner"}),
        ),
    )
    .await;
    let uid = f
        .state
        .store()
        .user_by_email("own@acme.test")
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        call(
            &f.app,
            &admin,
            "DELETE",
            &format!("/v1/orgs/acme/members/{uid}"),
            None
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &f.app,
            &admin,
            "PATCH",
            &format!("/v1/orgs/acme/members/{uid}"),
            Some(json!({"role": "viewer"}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
}
