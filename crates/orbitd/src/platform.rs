//! The platform endpoints: setup, login and sessions, users, organisations, members, projects, domains, routes, API
//! tokens and the audit log. Permissions are decided by [`Authz`] only. See `docs/platform.md` and `docs/api.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use oc_store::{Role, TokenScope, User, split_key};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, ApiResult, AppState};
use crate::auth::{
    self, Authz, MIN_PASSWORD, Perm, Principal, RateLimiter, SESSION_TTL, csrf_token,
    new_api_token, new_session_value, session_cookie,
};
use crate::gateway::dns::{DnsLookup, SystemDns};
use crate::gateway::domains;

/// Host-level settings and in-memory security state.
pub struct Platform {
    /// The operator's base domain, when the gateway is on.
    pub base_domain: Option<String>,
    /// The first-boot setup token, until a user exists.
    pub setup_token: Mutex<Option<String>>,
    /// Failed logins per client address and per email.
    pub limiter_login: RateLimiter,
    /// Failed API token authentications per client address.
    pub limiter_tokens: RateLimiter,
    /// Failed setup attempts per client address.
    pub limiter_setup: RateLimiter,
    /// DNS for domain verification.
    pub dns: Arc<dyn DnsLookup>,
    /// Newly verified hosts are sent here so a certificate is ordered.
    pub verified_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Platform")
            .field("base_domain", &self.base_domain)
            .finish_non_exhaustive()
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::new(None, Arc::new(SystemDns::default()))
    }
}

impl Platform {
    /// A platform with the given base domain and DNS.
    #[must_use]
    pub fn new(base_domain: Option<String>, dns: Arc<dyn DnsLookup>) -> Self {
        let window = Duration::from_secs(15 * 60);
        Self {
            base_domain,
            setup_token: Mutex::new(None),
            limiter_login: RateLimiter::new(5, window),
            limiter_tokens: RateLimiter::new(30, window),
            limiter_setup: RateLimiter::new(5, window),
            dns,
            verified_tx: Mutex::new(None),
        }
    }

    /// Generates the setup token when the node has no user yet; returns it.
    #[must_use]
    pub fn init_setup_token(&self, store: &oc_store::Store) -> Option<String> {
        if store.user_count().unwrap_or(1) > 0 {
            return None;
        }
        let t = oc_store::random_id(24);
        *self
            .setup_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(t.clone());
        Some(t)
    }

    fn host_of(&self, project: &str) -> Option<String> {
        self.base_domain.as_ref().map(|b| format!("{project}.{b}"))
    }
}

/// Routes below `/v1`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/me", get(me))
        .route("/users", get(list_users).post(create_user))
        .route("/orgs", get(list_orgs).post(create_org))
        .route("/orgs/{org}", get(get_org).delete(delete_org))
        .route("/orgs/{org}/members", get(list_members).post(add_member))
        .route(
            "/orgs/{org}/members/{user}",
            axum::routing::patch(update_member).delete(remove_member),
        )
        .route("/orgs/{org}/audit", get(org_audit))
        .route("/audit", get(global_audit))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project}",
            get(get_project).delete(delete_project),
        )
        .route(
            "/projects/{project}/domains",
            get(list_domains).post(add_domain),
        )
        .route(
            "/projects/{project}/domains/{host}",
            get(get_domain).delete(delete_domain),
        )
        .route(
            "/projects/{project}/domains/{host}/verify",
            post(verify_domain),
        )
        .route(
            "/projects/{project}/routes",
            get(list_routes).post(add_route),
        )
        .route("/projects/{project}/routes/{id}", delete(delete_route))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
}

// ------------------------------------------------------------------------------------------------ json

fn user_json(u: &User) -> Value {
    json!({ "id": u.id, "email": u.email, "display_name": u.display_name, "is_admin": u.is_admin, "created_at": u.created_at })
}

fn domain_json(d: &oc_store::Domain, base: Option<&str>) -> Value {
    json!({
        "host": d.host, "project": d.project, "status": d.status,
        "txt": { "name": domains::txt_name(&d.host), "value": d.txt_value },
        "cname": { "name": d.host, "value": base.map(|b| format!("{}.{b}", d.project)) },
        "cname_ok": d.cname_ok, "created_at": d.created_at, "verified_at": d.verified_at,
        "last_checked": d.last_checked, "last_error": d.last_error,
    })
}

fn route_json(r: &oc_store::Route) -> Value {
    json!({
        "id": r.id, "project": r.project, "host": (!r.host.is_empty()).then_some(&r.host),
        "path_prefix": r.path_prefix, "app": split_key(&r.app).1, "app_key": r.app,
    })
}

fn project_json(p: &oc_store::Project, platform: &Platform) -> Value {
    json!({
        "name": p.name, "org": p.org, "created_at": p.created_at,
        "host": platform.host_of(&p.name),
    })
}

fn parse_role(s: &str) -> ApiResult<Role> {
    Role::parse(s).ok_or_else(|| ApiError::bad("role must be viewer, developer, admin or owner"))
}

fn client_key(authz_ip: Option<std::net::IpAddr>) -> String {
    authz_ip.map_or_else(|| "unknown".to_owned(), |i| i.to_string())
}

fn system_audit(
    s: &AppState,
    action: &str,
    user: Option<&User>,
    label: &str,
    ip: Option<std::net::IpAddr>,
) {
    let entry = oc_store::NewAudit {
        actor_kind: "user".into(),
        actor_id: user.map(|u| u.id.clone()),
        actor_label: label.to_owned(),
        action: action.into(),
        ip: ip.map(|i| i.to_string()),
        ..oc_store::NewAudit::default()
    };
    if let Err(e) = s.store().audit(&entry) {
        tracing::error!("audit log write failed: {e}");
    }
}

fn with_cookie(mut r: Response, cookie: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

fn meta_of(headers: &HeaderMap, ext: &axum::http::Extensions) -> auth::RequestMeta {
    let _ = headers;
    ext.get::<auth::RequestMeta>().copied().unwrap_or_default()
}

// ------------------------------------------------------------------------------------------------ setup, login

/// `GET /setup`: a small form for the first owner. Part of the node, so it works before any console exists.
pub async fn setup_page() -> Html<&'static str> {
    Html(include_str!("setup.html"))
}

#[derive(Deserialize)]
struct SetupBody {
    token: String,
    email: String,
    display_name: Option<String>,
    password: String,
}

async fn start_session(
    s: &AppState,
    user: &User,
    meta: auth::RequestMeta,
    headers: &HeaderMap,
) -> ApiResult<(String, String)> {
    let value = new_session_value();
    let ttl = i64::try_from(SESSION_TTL.as_millis()).unwrap_or(i64::MAX);
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let ip = meta.ip.map(|i| i.to_string());
    s.store().create_session(
        &oc_store::sha256_hex(&value),
        &user.id,
        ttl,
        ip.as_deref(),
        ua,
    )?;
    let cookie = session_cookie(Some(&value), meta.https);
    Ok((csrf_token(&value), cookie))
}

fn me_body(s: &AppState, user: &User, csrf: Option<&str>) -> ApiResult<Value> {
    let orgs: Vec<Value> = s
        .store()
        .memberships_of(&user.id)?
        .into_iter()
        .map(|(o, r)| json!({ "org": o.name, "role": r }))
        .collect();
    Ok(json!({ "user": user_json(user), "memberships": orgs, "csrf_token": csrf }))
}

async fn setup(
    State(s): State<AppState>,
    req_parts: axum::http::Request<axum::body::Body>,
) -> ApiResult<Response> {
    let (parts, body) = req_parts.into_parts();
    let meta = meta_of(&parts.headers, &parts.extensions);
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .map_err(|_| ApiError::bad("body too large"))?;
    let b: SetupBody =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::bad(format!("body: {e}")))?;
    let key = format!("setup:{}", client_key(meta.ip));
    s.platform
        .limiter_setup
        .check(&key)
        .map_err(auth::too_many)?;
    if s.store().user_count()? > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "setup is done: a user exists already",
        ));
    }
    let expected = s
        .platform
        .setup_token
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let admin_bearer = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .zip(s.token.as_deref())
        .is_some_and(|(a, t)| auth::constant_time_eq(a.as_bytes(), t.as_bytes()));
    let token_ok = expected
        .as_deref()
        .is_some_and(|e| auth::constant_time_eq(b.token.as_bytes(), e.as_bytes()));
    if !token_ok && !admin_bearer {
        s.platform.limiter_setup.fail(&key);
        return Err(ApiError::new(StatusCode::FORBIDDEN, "wrong setup token"));
    }
    if b.password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad(format!(
            "the password needs at least {MIN_PASSWORD} characters"
        )));
    }
    let pw = b.password.clone();
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&pw))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let user =
        s.store()
            .create_first_user(&b.email, b.display_name.as_deref().unwrap_or(""), &hash)?;
    *s.platform
        .setup_token
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    system_audit(&s, "setup.complete", Some(&user), &user.email, meta.ip);
    let (csrf, cookie) = start_session(&s, &user, meta, &parts.headers).await?;
    let body = me_body(&s, &user, Some(&csrf))?;
    Ok(with_cookie(
        (StatusCode::CREATED, Json(body)).into_response(),
        &cookie,
    ))
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn login(
    State(s): State<AppState>,
    req: axum::http::Request<axum::body::Body>,
) -> ApiResult<Response> {
    let (parts, body) = req.into_parts();
    let meta = meta_of(&parts.headers, &parts.extensions);
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .map_err(|_| ApiError::bad("body too large"))?;
    let b: LoginBody =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::bad(format!("body: {e}")))?;
    let email = b.email.trim().to_lowercase();
    let ip_key = format!("login:ip:{}", client_key(meta.ip));
    let email_key = format!("login:email:{email}");
    for k in [&ip_key, &email_key] {
        s.platform.limiter_login.check(k).map_err(auth::too_many)?;
    }
    let user = s.store().user_by_email(&email)?;
    let ok = auth::check_login(user, b.password).await;
    let Some(user) = ok else {
        s.platform.limiter_login.fail(&ip_key);
        s.platform.limiter_login.fail(&email_key);
        system_audit(&s, "user.login_failed", None, &email, meta.ip);
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid email or password",
        ));
    };
    s.platform.limiter_login.reset(&email_key);
    system_audit(&s, "user.login", Some(&user), &user.email, meta.ip);
    let (csrf, cookie) = start_session(&s, &user, meta, &parts.headers).await?;
    let body = me_body(&s, &user, Some(&csrf))?;
    Ok(with_cookie(Json(body).into_response(), &cookie))
}

async fn logout(State(s): State<AppState>, authz: Authz) -> ApiResult<Response> {
    if let Principal::Session { value, .. } = &authz.principal {
        s.store().delete_session(&oc_store::sha256_hex(value))?;
        authz.audit("user.logout", None, None, None, None);
    }
    Ok(with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        &session_cookie(None, authz.meta.https),
    ))
}

async fn me(State(s): State<AppState>, authz: Authz) -> ApiResult<Json<Value>> {
    Ok(Json(match &authz.principal {
        Principal::Session { user, value } => me_body(&s, user, Some(&csrf_token(value)))?,
        Principal::Admin => json!({ "admin_token": true }),
        Principal::Token(t) => json!({ "token": token_json(&s, t) }),
    }))
}

// ------------------------------------------------------------------------------------------------ users

#[derive(Deserialize)]
struct NewUser {
    email: String,
    display_name: Option<String>,
    password: String,
    /// Optionally make the user a member of this organisation.
    org: Option<String>,
    role: Option<String>,
    is_admin: Option<bool>,
}

async fn list_users(State(s): State<AppState>, authz: Authz) -> ApiResult<Json<Value>> {
    authz.require_instance()?;
    Ok(Json(json!(
        s.store().users()?.iter().map(user_json).collect::<Vec<_>>()
    )))
}

async fn create_user(
    State(s): State<AppState>,
    authz: Authz,
    Json(b): Json<NewUser>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authz.require_instance()?;
    if b.password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad(format!(
            "the password needs at least {MIN_PASSWORD} characters"
        )));
    }
    let org = b
        .org
        .as_deref()
        .map(|o| authz.require_org(o, Perm::Manage))
        .transpose()?;
    let role = b
        .role
        .as_deref()
        .map(parse_role)
        .transpose()?
        .unwrap_or(Role::Developer);
    let pw = b.password.clone();
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&pw))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let user = s.store().create_user(
        &b.email,
        b.display_name.as_deref().unwrap_or(""),
        Some(&hash),
        b.is_admin.unwrap_or(false),
    )?;
    if let Some(o) = &org {
        s.store().set_member(&o.id, &user.id, role)?;
    }
    authz.audit(
        "user.create",
        Some(&user.email),
        org.as_ref().map(|o| o.id.as_str()),
        None,
        None,
    );
    Ok((StatusCode::CREATED, Json(user_json(&user))))
}

// ------------------------------------------------------------------------------------------------ orgs

/// The organisations the caller can see.
fn visible_orgs(s: &AppState, authz: &Authz) -> ApiResult<Vec<(oc_store::Org, Role)>> {
    let all = s.store().orgs()?;
    let mut out = Vec::new();
    for o in all {
        if let Some(r) = authz.role_in_org(&o)? {
            out.push((o, r));
        }
    }
    Ok(out)
}

async fn list_orgs(State(s): State<AppState>, authz: Authz) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        visible_orgs(&s, &authz)?
            .into_iter()
            .map(|(o, r)| json!({ "name": o.name, "created_at": o.created_at, "role": r }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
struct NewOrg {
    name: String,
}

async fn create_org(
    State(s): State<AppState>,
    authz: Authz,
    Json(b): Json<NewOrg>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authz.require_instance()?;
    let org = s.store().create_org(&b.name)?;
    if let Some(u) = authz.user() {
        s.store().set_member(&org.id, &u.id, Role::Owner)?;
    }
    authz.audit("org.create", Some(&org.name), Some(&org.id), None, None);
    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": org.name, "created_at": org.created_at })),
    ))
}

async fn get_org(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let org = authz.require_org(&name, Perm::Read)?;
    let role = authz.role_in_org(&org)?;
    let projects: Vec<Value> = s
        .store()
        .projects(Some(&org.name))?
        .iter()
        .map(|p| project_json(p, &s.platform))
        .collect();
    Ok(Json(
        json!({ "name": org.name, "created_at": org.created_at, "role": role, "projects": projects }),
    ))
}

async fn delete_org(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let org = authz.require_org(&name, Perm::Own)?;
    if org.name == oc_store::DEFAULT_PROJECT {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the default organisation cannot be deleted",
        ));
    }
    s.store().delete_org(&org.name)?;
    authz.audit("org.delete", Some(&org.name), Some(&org.id), None, None);
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------ members

async fn list_members(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let org = authz.require_org(&name, Perm::Read)?;
    Ok(Json(json!(s.store().members(&org.id)?)))
}

#[derive(Deserialize)]
struct MemberBody {
    email: Option<String>,
    role: String,
}

/// Only owners may hand out or change ownership.
fn check_grant(
    authz: &Authz,
    org: &oc_store::Org,
    role: Role,
    target_current: Option<Role>,
) -> ApiResult<()> {
    if (role == Role::Owner || target_current == Some(Role::Owner))
        && authz.role_in_org(org)? != Some(Role::Owner)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only an owner can grant or change ownership",
        ));
    }
    Ok(())
}

async fn add_member(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
    Json(b): Json<MemberBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let org = authz.require_org(&name, Perm::Manage)?;
    let role = parse_role(&b.role)?;
    let email = b.email.ok_or_else(|| ApiError::bad("email is required"))?;
    let user = s.store().user_by_email(&email)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no user with email `{email}`; create the user first"),
        )
    })?;
    let current = s.store().role_of(&user.id, &org.id)?;
    check_grant(&authz, &org, role, current)?;
    s.store().set_member(&org.id, &user.id, role)?;
    authz.audit(
        "member.set",
        Some(&user.email),
        Some(&org.id),
        None,
        Some(json!({ "role": role })),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user_id": user.id, "email": user.email, "role": role })),
    ))
}

async fn update_member(
    State(s): State<AppState>,
    authz: Authz,
    Path((name, uid)): Path<(String, String)>,
    Json(b): Json<MemberBody>,
) -> ApiResult<Json<Value>> {
    let org = authz.require_org(&name, Perm::Manage)?;
    let role = parse_role(&b.role)?;
    let current = s
        .store()
        .role_of(&uid, &org.id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not a member"))?;
    check_grant(&authz, &org, role, Some(current))?;
    if current == Role::Owner && role != Role::Owner && s.store().owner_count(&org.id)? <= 1 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "an organisation needs at least one owner",
        ));
    }
    s.store().set_member(&org.id, &uid, role)?;
    authz.audit(
        "member.set",
        Some(&uid),
        Some(&org.id),
        None,
        Some(json!({ "role": role })),
    );
    Ok(Json(json!({ "user_id": uid, "role": role })))
}

async fn remove_member(
    State(s): State<AppState>,
    authz: Authz,
    Path((name, uid)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = authz.require_org(&name, Perm::Manage)?;
    let current = s
        .store()
        .role_of(&uid, &org.id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not a member"))?;
    check_grant(&authz, &org, current, Some(current))?;
    if current == Role::Owner && s.store().owner_count(&org.id)? <= 1 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "an organisation needs at least one owner",
        ));
    }
    s.store().remove_member(&org.id, &uid)?;
    authz.audit("member.remove", Some(&uid), Some(&org.id), None, None);
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------ audit

#[derive(Deserialize, Default)]
struct AuditParams {
    before: Option<i64>,
    limit: Option<i64>,
    project: Option<String>,
}

async fn org_audit(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
    Query(q): Query<AuditParams>,
) -> ApiResult<Json<Value>> {
    let org = authz.require_org(&name, Perm::Manage)?;
    let project_id = match q.project.as_deref() {
        Some(p) => {
            let p = authz.require_project(p, Perm::Manage)?;
            if p.org_id != org.id {
                return Err(ApiError::bad("the project is not in this organisation"));
            }
            Some(p.id)
        }
        None => None,
    };
    Ok(Json(json!(s.store().audit_log(
        Some(&org.id),
        project_id.as_deref(),
        q.before,
        q.limit.unwrap_or(100)
    )?)))
}

async fn global_audit(
    State(s): State<AppState>,
    authz: Authz,
    Query(q): Query<AuditParams>,
) -> ApiResult<Json<Value>> {
    authz.require_instance()?;
    Ok(Json(json!(s.store().audit_log(
        None,
        None,
        q.before,
        q.limit.unwrap_or(100)
    )?)))
}

// ------------------------------------------------------------------------------------------------ projects

async fn list_projects(State(s): State<AppState>, authz: Authz) -> ApiResult<Json<Value>> {
    let mut out = Vec::new();
    for p in s.store().projects(None)? {
        if authz.role_in_project(&p)?.is_some() {
            out.push(project_json(&p, &s.platform));
        }
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
struct NewProject {
    org: Option<String>,
    name: String,
}

async fn create_project(
    State(s): State<AppState>,
    authz: Authz,
    Json(b): Json<NewProject>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let org_name = match b.org {
        Some(o) => o,
        None => {
            // a caller with exactly one organisation does not have to name it
            let orgs = visible_orgs(&s, &authz)?;
            match orgs.as_slice() {
                [(o, _)] => o.name.clone(),
                _ => return Err(ApiError::bad("org is required")),
            }
        }
    };
    let org = authz.require_org(&org_name, Perm::Manage)?;
    let p = s.store().create_project(&org.name, &b.name)?;
    authz.audit(
        "project.create",
        Some(&p.name),
        Some(&org.id),
        Some(&p.id),
        None,
    );
    Ok((StatusCode::CREATED, Json(project_json(&p, &s.platform))))
}

async fn get_project(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let p = authz.require_project(&name, Perm::Read)?;
    let role = authz.role_in_project(&p)?;
    let mut v = project_json(&p, &s.platform);
    v["role"] = json!(role);
    v["apps"] = json!(s.store().apps_in(&p.name)?.len());
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
struct DeleteProject {
    force: Option<bool>,
}

async fn delete_project(
    State(s): State<AppState>,
    authz: Authz,
    Path(name): Path<String>,
    Query(q): Query<DeleteProject>,
) -> ApiResult<StatusCode> {
    let p = authz.require_project(&name, Perm::Own)?;
    if q.force == Some(true) {
        for a in s.store().apps_in(&p.name)? {
            crate::api::remove_app(&s, &a.name).await?;
        }
    }
    s.store().delete_project(&p.name)?;
    authz.audit(
        "project.delete",
        Some(&p.name),
        Some(&p.org_id),
        Some(&p.id),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------ domains

async fn list_domains(
    State(s): State<AppState>,
    authz: Authz,
    Path(project): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&project, Perm::Read)?;
    let base = s.platform.base_domain.as_deref();
    Ok(Json(json!(
        s.store()
            .domains(&project)?
            .iter()
            .map(|d| domain_json(d, base))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
struct NewDomain {
    host: String,
}

async fn add_domain(
    State(s): State<AppState>,
    authz: Authz,
    Path(project): Path<String>,
    Json(b): Json<NewDomain>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = authz.require_project(&project, Perm::Manage)?;
    let host = domains::validate_domain(&b.host, s.platform.base_domain.as_deref())
        .map_err(ApiError::bad)?;
    let d = s.store().add_domain(&p.name, &host)?;
    authz.audit(
        "domain.add",
        Some(&host),
        Some(&p.org_id),
        Some(&p.id),
        None,
    );
    // first check right away (the customer may have set the records before adding the domain)
    let st = s.clone();
    tokio::spawn(async move { background_verify(&st, &d).await });
    let d = s.store().domain(&p.name, &host)?;
    Ok((
        StatusCode::CREATED,
        Json(domain_json(&d, s.platform.base_domain.as_deref())),
    ))
}

async fn background_verify(s: &AppState, d: &oc_store::Domain) -> bool {
    match domains::verify_and_record(
        s.store(),
        s.platform.dns.as_ref(),
        s.platform.base_domain.as_deref(),
        d,
    )
    .await
    {
        Ok(true) => {
            if let Some(tx) = s
                .platform
                .verified_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                let _ = tx.send(d.host.clone());
            }
            true
        }
        _ => false,
    }
}

async fn get_domain(
    State(s): State<AppState>,
    authz: Authz,
    Path((project, host)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&project, Perm::Read)?;
    let d = s.store().domain(&project, &host.to_lowercase())?;
    Ok(Json(domain_json(&d, s.platform.base_domain.as_deref())))
}

async fn verify_domain(
    State(s): State<AppState>,
    authz: Authz,
    Path((project, host)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let p = authz.require_project(&project, Perm::Manage)?;
    let d = s.store().domain(&p.name, &host.to_lowercase())?;
    if d.status == "pending" {
        let verified = background_verify(&s, &d).await;
        if verified {
            authz.audit(
                "domain.verify",
                Some(&d.host),
                Some(&p.org_id),
                Some(&p.id),
                None,
            );
        }
    }
    let d = s.store().domain(&p.name, &host.to_lowercase())?;
    Ok(Json(domain_json(&d, s.platform.base_domain.as_deref())))
}

async fn delete_domain(
    State(s): State<AppState>,
    authz: Authz,
    Path((project, host)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let p = authz.require_project(&project, Perm::Manage)?;
    s.store().delete_domain(&p.name, &host.to_lowercase())?;
    authz.audit(
        "domain.remove",
        Some(&host),
        Some(&p.org_id),
        Some(&p.id),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------ routes

async fn list_routes(
    State(s): State<AppState>,
    authz: Authz,
    Path(project): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&project, Perm::Read)?;
    Ok(Json(json!(
        s.store()
            .routes(&project)?
            .iter()
            .map(route_json)
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
struct NewRoute {
    host: Option<String>,
    path_prefix: String,
    app: String,
}

async fn add_route(
    State(s): State<AppState>,
    authz: Authz,
    Path(project): Path<String>,
    Json(b): Json<NewRoute>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = authz.require_project(&project, Perm::Write)?;
    if !oc_store::valid_name(&b.app) {
        return Err(ApiError::bad("invalid app name"));
    }
    let key = oc_store::key_of(&p.name, &b.app);
    let app = s.store().app(&key)?;
    if s.store().app_port(&app.name)?.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "`{}` is not an HTTP service: deploy it with \"http\": true",
                b.app
            ),
        ));
    }
    let host = match b.host.as_deref().filter(|h| !h.is_empty()) {
        None => String::new(),
        Some(h) => {
            let h = h.trim().to_lowercase();
            let own_host = s.platform.host_of(&p.name);
            let is_domain = s.store().domains(&p.name)?.iter().any(|d| d.host == h);
            if own_host.as_deref() != Some(h.as_str()) && !is_domain {
                return Err(ApiError::bad(format!(
                    "`{h}` is neither this project's host nor one of its custom domains"
                )));
            }
            h
        }
    };
    let r = s.store().add_route(&p.name, &host, &b.path_prefix, &key)?;
    authz.audit(
        "route.add",
        Some(&b.path_prefix),
        Some(&p.org_id),
        Some(&p.id),
        Some(json!({ "app": key, "host": host })),
    );
    Ok((StatusCode::CREATED, Json(route_json(&r))))
}

async fn delete_route(
    State(s): State<AppState>,
    authz: Authz,
    Path((project, id)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    let p = authz.require_project(&project, Perm::Write)?;
    s.store().delete_route(&p.name, id)?;
    authz.audit(
        "route.remove",
        Some(&id.to_string()),
        Some(&p.org_id),
        Some(&p.id),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------ tokens

fn token_json(s: &AppState, t: &oc_store::ApiToken) -> Value {
    let scope = match t.scope() {
        TokenScope::Org(id) => {
            json!({ "org": s.store().orgs().ok().and_then(|o| o.into_iter().find(|o| o.id == id)).map(|o| o.name) })
        }
        TokenScope::Project(id) => {
            json!({ "project": s.store().projects(None).ok().and_then(|p| p.into_iter().find(|p| p.id == id)).map(|p| p.name) })
        }
    };
    json!({
        "id": t.id, "prefix": t.prefix, "name": t.name, "scope": scope, "role": t.role,
        "expires_at": t.expires_at, "created_at": t.created_at, "last_used": t.last_used,
        "revoked_at": t.revoked_at, "user_id": t.user_id,
    })
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
    scope: HashMap<String, String>,
    role: String,
    expires_in_days: Option<i64>,
}

async fn create_token(
    State(s): State<AppState>,
    authz: Authz,
    Json(b): Json<NewToken>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if matches!(authz.principal, Principal::Token(_)) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "API tokens cannot create tokens",
        ));
    }
    let role = parse_role(&b.role)?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("name is required"));
    }
    let (scope, org_id, project_id, have) = match (b.scope.get("org"), b.scope.get("project")) {
        (Some(o), None) => {
            let org = authz.require_org(o, Perm::Read)?;
            let have = authz.role_in_org(&org)?;
            (TokenScope::Org(org.id.clone()), Some(org.id), None, have)
        }
        (None, Some(p)) => {
            let pr = authz.require_project(p, Perm::Read)?;
            let have = authz.role_in_project(&pr)?;
            (
                TokenScope::Project(pr.id.clone()),
                Some(pr.org_id),
                Some(pr.id),
                have,
            )
        }
        _ => {
            return Err(ApiError::bad(
                "scope must be {\"org\": name} or {\"project\": name}",
            ));
        }
    };
    if have.is_none_or(|h| h < role) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "a token cannot have a higher role than its creator",
        ));
    }
    let expires = b
        .expires_in_days
        .map(|d| {
            if d <= 0 {
                Err(ApiError::bad("expires_in_days must be positive"))
            } else {
                Ok(oc_store::now_ms().saturating_add(d.saturating_mul(86_400_000)))
            }
        })
        .transpose()?;
    let (secret, prefix, hash) = new_api_token();
    let user_id = authz.user().map(|u| u.id.clone());
    let t = s.store().create_token(
        b.name.trim(),
        &prefix,
        &hash,
        user_id.as_deref(),
        &scope,
        role,
        expires,
    )?;
    authz.audit(
        "token.create",
        Some(&t.id),
        org_id.as_deref(),
        project_id.as_deref(),
        Some(json!({ "name": t.name, "role": role })),
    );
    let mut v = token_json(&s, &t);
    v["token"] = json!(secret);
    Ok((StatusCode::CREATED, Json(v)))
}

async fn list_tokens(State(s): State<AppState>, authz: Authz) -> ApiResult<Json<Value>> {
    let only_user = match &authz.principal {
        Principal::Admin => None,
        Principal::Session { user, .. } if user.is_admin => None,
        Principal::Session { user, .. } => Some(user.id.clone()),
        Principal::Token(_) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "API tokens cannot list tokens",
            ));
        }
    };
    Ok(Json(json!(
        s.store()
            .tokens(only_user.as_deref())?
            .iter()
            .map(|t| token_json(&s, t))
            .collect::<Vec<_>>()
    )))
}

async fn revoke_token(
    State(s): State<AppState>,
    authz: Authz,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let t = s.store().token(&id)?;
    let mine = authz
        .user()
        .is_some_and(|u| t.user_id.as_deref() == Some(u.id.as_str()));
    if !mine && authz.require_instance().is_err() {
        // not yours: indistinguishable from a token that does not exist
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("token {id} not found"),
        ));
    }
    s.store().revoke_token(&id)?;
    authz.audit("token.revoke", Some(&id), None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
