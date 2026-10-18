//! Authentication and the one place that authorises: the [`Authz`] extractor.
//!
//! Every `/v1` handler takes an `Authz` (which authenticates the request or answers 401) and asks it a question with
//! [`Authz::require_project`], [`Authz::require_org`] or [`Authz::require_instance`]. Handlers never inspect headers or
//! roles themselves. See `docs/platform.md`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use oc_store::{ApiToken, NewAudit, Org, Project, Role, Store, TokenScope, User, sha256_hex};

use crate::api::{ApiError, AppState};

/// Name of the session cookie (with the `__Host-` prefix on HTTPS).
pub const SESSION_COOKIE: &str = "orbit_session";
/// Session lifetime.
pub const SESSION_TTL: Duration = Duration::from_secs(14 * 24 * 3600);
/// Prefix of API tokens.
pub const TOKEN_PREFIX: &str = "oc_";
/// Shortest accepted password.
pub const MIN_PASSWORD: usize = 10;

/// Facts about the connection a request arrived on, set by the gateway (after resolving trusted proxies) or by the
/// direct listener.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestMeta {
    /// The client address, when known.
    pub ip: Option<IpAddr>,
    /// Whether the client spoke HTTPS (directly, or to a trusted proxy that said so).
    pub https: bool,
}

// ------------------------------------------------------------------------------------------------ passwords

/// Hashes a password with argon2id; returns the PHC string.
///
/// # Errors
/// When hashing fails (it does not for valid input).
pub fn hash_password(password: &str) -> Result<String, String> {
    let h: PasswordHash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(h.to_string())
}

/// Whether `password` matches the PHC string `hash`.
#[must_use]
pub fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// A hash to verify against when the user does not exist, so that the response time does not tell.
fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_password("orbit-dummy-password").unwrap_or_default())
}

/// Checks a login without revealing whether the email exists. Runs on the blocking pool.
pub async fn check_login(user: Option<User>, password: String) -> Option<User> {
    tokio::task::spawn_blocking(move || {
        let hash = user
            .as_ref()
            .and_then(|u| u.password_hash.clone())
            .unwrap_or_else(|| dummy_hash().to_owned());
        let ok = verify_password(&password, &hash);
        user.filter(|u| ok && !u.disabled && u.password_hash.is_some())
    })
    .await
    .ok()
    .flatten()
}

// ------------------------------------------------------------------------------------------------ rate limit

/// Counts failures per key in a sliding window.
#[derive(Debug)]
pub struct RateLimiter {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    /// At most `max` failures per `window` per key.
    #[must_use]
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Instant>>> {
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ok while the key may try; the error is the time to wait.
    ///
    /// # Errors
    /// The remaining wait when the key is over its limit.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut m = self.map();
        let Some(v) = m.get_mut(key) else {
            return Ok(());
        };
        v.retain(|t| now.duration_since(*t) < self.window);
        if v.len() >= self.max {
            let oldest = v.first().copied().unwrap_or(now);
            return Err(self.window.saturating_sub(now.duration_since(oldest)));
        }
        Ok(())
    }

    /// Records a failure.
    pub fn fail(&self, key: &str) {
        let now = Instant::now();
        let mut m = self.map();
        if m.len() > 10_000 {
            m.retain(|_, v| {
                v.last()
                    .is_some_and(|t| now.duration_since(*t) < self.window)
            });
        }
        m.entry(key.to_owned()).or_default().push(now);
    }

    /// Forgets a key (after a success).
    pub fn reset(&self, key: &str) {
        self.map().remove(key);
    }
}

/// The 429 answer for a limited key.
#[must_use]
pub fn too_many(wait: Duration) -> ApiError {
    ApiError::with_header(
        StatusCode::TOO_MANY_REQUESTS,
        "too many failed attempts; try again later",
        "retry-after",
        wait.as_secs().max(1).to_string(),
    )
}

// ------------------------------------------------------------------------------------------------ cookies, csrf

/// The CSRF token of a session: derived from the session value, which scripts cannot read (`HttpOnly`).
#[must_use]
pub fn csrf_token(session_value: &str) -> String {
    sha256_hex(&format!("orbit-csrf:{session_value}"))
}

/// A new random session value.
#[must_use]
pub fn new_session_value() -> String {
    oc_store::random_id(32)
}

/// The `Set-Cookie` value that sets (`Some`) or clears (`None`) the session.
#[must_use]
pub fn session_cookie(value: Option<&str>, https: bool) -> String {
    let name = if https {
        format!("__Host-{SESSION_COOKIE}")
    } else {
        SESSION_COOKIE.to_owned()
    };
    let (v, age) = match value {
        Some(v) => (v, SESSION_TTL.as_secs()),
        None => ("", 0),
    };
    format!(
        "{name}={v}; Path=/; HttpOnly; SameSite=Lax; Max-Age={age}{}",
        if https { "; Secure" } else { "" }
    )
}

/// The session value in a `Cookie` header, if any.
#[must_use]
pub fn cookie_session(cookie_header: &str) -> Option<String> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == SESSION_COOKIE || k == format!("__Host-{SESSION_COOKIE}"))
            .then(|| v.to_owned())
            .filter(|v| !v.is_empty())
    })
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Creates the secret of an API token: `(secret, prefix, hash)`.
#[must_use]
pub fn new_api_token() -> (String, String, String) {
    let secret = format!("{TOKEN_PREFIX}{}", oc_store::random_id(32));
    let prefix = secret.chars().take(10).collect();
    let hash = sha256_hex(&secret);
    (secret, prefix, hash)
}

// ------------------------------------------------------------------------------------------------ principals

/// Who is calling.
#[derive(Clone, Debug)]
pub enum Principal {
    /// The break-glass token, or an open node (no token configured and no user yet).
    Admin,
    /// A signed-in user, through a console session.
    Session {
        /// The user.
        user: User,
        /// The cookie value (to derive the CSRF token and to log out).
        value: String,
    },
    /// A user acting with their own bearer credential is not distinct from a token; this is an API token.
    Token(ApiToken),
}

/// What an action needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    /// Look.
    Read,
    /// Change apps, data and routes.
    Write,
    /// Domains, tokens, members.
    Manage,
    /// Destructive organisation-level operations.
    Own,
}

impl Perm {
    /// The lowest role that has this permission.
    #[must_use]
    pub fn min_role(self) -> Role {
        match self {
            Perm::Read => Role::Viewer,
            Perm::Write => Role::Developer,
            Perm::Manage => Role::Admin,
            Perm::Own => Role::Owner,
        }
    }
}

/// The authenticated caller and the single place where permissions are decided.
#[derive(Clone, Debug)]
pub struct Authz {
    /// Who.
    pub principal: Principal,
    /// Connection facts.
    pub meta: RequestMeta,
    store: Arc<Store>,
}

fn forbidden(what: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, format!("not allowed: {what}"))
}

impl Authz {
    /// An authz for tests and internal use.
    #[must_use]
    pub fn new(principal: Principal, meta: RequestMeta, store: Arc<Store>) -> Self {
        Self {
            principal,
            meta,
            store,
        }
    }

    /// The signed-in user, when the principal is a session.
    #[must_use]
    pub fn user(&self) -> Option<&User> {
        match &self.principal {
            Principal::Session { user, .. } => Some(user),
            _ => None,
        }
    }

    /// Whether this is the break-glass admin.
    #[must_use]
    pub fn is_break_glass(&self) -> bool {
        matches!(self.principal, Principal::Admin)
    }

    /// The effective role in an organisation (and, for project-scoped tokens, a project).
    fn role_in(&self, org_id: &str, project_id: Option<&str>) -> Result<Option<Role>, ApiError> {
        Ok(match &self.principal {
            Principal::Admin => Some(Role::Owner),
            Principal::Session { user, .. } => self.user_role(user, org_id)?,
            Principal::Token(t) => {
                let in_scope = match t.scope() {
                    TokenScope::Org(id) => id == org_id,
                    TokenScope::Project(id) => project_id == Some(id.as_str()),
                };
                if !in_scope {
                    return Ok(None);
                }
                // A token never outlives its creator's powers: it is capped by their current role.
                match &t.user_id {
                    None => Some(t.role),
                    Some(uid) => {
                        let user = self.store.user(uid)?;
                        self.user_role(&user, org_id)?.map(|r| r.min(t.role))
                    }
                }
            }
        })
    }

    fn user_role(&self, user: &User, org_id: &str) -> Result<Option<Role>, ApiError> {
        if user.disabled {
            return Ok(None);
        }
        if user.is_admin {
            return Ok(Some(Role::Owner)); // the operator owns every organisation
        }
        Ok(self.store.role_of(&user.id, org_id)?)
    }

    /// The role the caller has in an organisation, if any.
    ///
    /// # Errors
    /// Store failures.
    pub fn role_in_org(&self, org: &Org) -> Result<Option<Role>, ApiError> {
        self.role_in(&org.id, None)
    }

    /// The role the caller has in a project, if any.
    ///
    /// # Errors
    /// Store failures.
    pub fn role_in_project(&self, p: &Project) -> Result<Option<Role>, ApiError> {
        self.role_in(&p.org_id, Some(&p.id))
    }

    /// Instance-wide operations: the break-glass admin or a user with `is_admin`.
    ///
    /// # Errors
    /// 403.
    pub fn require_instance(&self) -> Result<(), ApiError> {
        match &self.principal {
            Principal::Admin => Ok(()),
            Principal::Session { user, .. } if user.is_admin && !user.disabled => Ok(()),
            _ => Err(forbidden("this needs an instance administrator")),
        }
    }

    /// The organisation `name`, if the caller may do `perm` in it. A caller with no role at all gets 404.
    ///
    /// # Errors
    /// 404 (unknown or invisible), 403 (visible but not enough).
    pub fn require_org(&self, name: &str, perm: Perm) -> Result<Org, ApiError> {
        let org = self.store.org(name).map_err(|_| not_found_org(name))?;
        match self.role_in_org(&org)? {
            None => Err(not_found_org(name)),
            Some(r) if r >= perm.min_role() => Ok(org),
            Some(_) => Err(forbidden(&format!("{perm:?} on organisation `{name}`"))),
        }
    }

    /// The project `name`, if the caller may do `perm` in it. A caller with no role at all gets 404, so project
    /// names cannot be probed.
    ///
    /// # Errors
    /// 404 (unknown or invisible), 403 (visible but not enough).
    pub fn require_project(&self, name: &str, perm: Perm) -> Result<Project, ApiError> {
        let p = self
            .store
            .project(name)
            .map_err(|_| not_found_project(name))?;
        match self.role_in_project(&p)? {
            None => Err(not_found_project(name)),
            Some(r) if r >= perm.min_role() => Ok(p),
            Some(_) => Err(forbidden(&format!("{perm:?} on project `{name}`"))),
        }
    }

    /// Same as [`Authz::require_project`], for an object addressed by app key (a run, a trace).
    ///
    /// # Errors
    /// As [`Authz::require_project`].
    pub fn require_app_key(&self, key: &str, perm: Perm) -> Result<Project, ApiError> {
        self.require_project(oc_store::split_key(key).0, perm)
    }

    /// Writes an audit entry for an action of the caller.
    pub fn audit(
        &self,
        action: &str,
        target: Option<&str>,
        org_id: Option<&str>,
        project_id: Option<&str>,
        detail: Option<serde_json::Value>,
    ) {
        let (actor_kind, actor_id, actor_label) = match &self.principal {
            Principal::Admin => ("admin-token", None, "admin-token".to_owned()),
            Principal::Session { user, .. } => ("user", Some(user.id.clone()), user.email.clone()),
            Principal::Token(t) => ("token", Some(t.id.clone()), t.name.clone()),
        };
        let entry = NewAudit {
            actor_kind: actor_kind.into(),
            actor_id,
            actor_label,
            action: action.into(),
            target: target.map(str::to_owned),
            org_id: org_id.map(str::to_owned),
            project_id: project_id.map(str::to_owned),
            detail,
            ip: self.meta.ip.map(|i| i.to_string()),
        };
        if let Err(e) = self.store.audit(&entry) {
            tracing::error!("audit log write failed: {e}");
        }
    }
}

fn not_found_org(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        format!("organisation `{name}` not found"),
    )
}

fn not_found_project(name: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, format!("project `{name}` not found"))
}

fn unauthorized(msg: &str) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, msg)
}

fn host_of_origin(origin: &str) -> &str {
    origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .trim_end_matches('/')
}

impl FromRequestParts<AppState> for Authz {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let meta = parts
            .extensions
            .get::<RequestMeta>()
            .copied()
            .unwrap_or_default();
        let store = Arc::clone(state.node.executor.store());
        let limiter_key = format!(
            "badtoken:{}",
            meta.ip.map_or_else(String::new, |i| i.to_string())
        );
        let mk = |principal| Authz {
            principal,
            meta,
            store: Arc::clone(&store),
        };

        // 1. bearer token (or ?token= for EventSource clients that cannot set headers)
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned)
            .or_else(|| {
                parts.uri.query().and_then(|q| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix("token=").map(str::to_owned))
                })
            });
        if let Some(b) = bearer {
            if let Some(admin) = state.token.as_deref()
                && constant_time_eq(b.as_bytes(), admin.as_bytes())
            {
                return Ok(mk(Principal::Admin));
            }
            if let Err(wait) = state.platform.limiter_tokens.check(&limiter_key) {
                return Err(too_many(wait));
            }
            if b.starts_with(TOKEN_PREFIX)
                && let Some(t) = store.token_by_hash(&sha256_hex(&b))?
            {
                return Ok(mk(Principal::Token(t)));
            }
            state.platform.limiter_tokens.fail(&limiter_key);
            return Err(unauthorized("invalid or expired token"));
        }

        // 2. session cookie
        let cookie = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(cookie_session);
        if let Some(value) = cookie {
            let ttl = i64::try_from(SESSION_TTL.as_millis()).unwrap_or(i64::MAX);
            let hash = sha256_hex(&value);
            let session = store
                .session(&hash, ttl)?
                .ok_or_else(|| unauthorized("session expired; sign in again"))?;
            let user = store.user(&session.user_id)?;
            if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
                // CSRF: a custom header only the console can read from /v1/me, and a same-origin Origin.
                if let Some(origin) = parts
                    .headers
                    .get(header::ORIGIN)
                    .and_then(|v| v.to_str().ok())
                {
                    let host = parts
                        .headers
                        .get(header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    if !host_of_origin(origin).eq_ignore_ascii_case(host) {
                        return Err(forbidden("cross-origin request"));
                    }
                }
                let sent = parts
                    .headers
                    .get("x-csrf-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                if !constant_time_eq(sent.as_bytes(), csrf_token(&value).as_bytes()) {
                    return Err(forbidden("missing or wrong X-CSRF-Token"));
                }
            }
            return Ok(mk(Principal::Session { user, value }));
        }

        // 3. nothing: only an open node (no admin token, no users yet) lets this through
        if state.token.is_none() && store.user_count()? == 0 {
            return Ok(mk(Principal::Admin));
        }
        Err(unauthorized(
            "authentication required: sign in, or send Authorization: Bearer <token>",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_roundtrip() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn limiter_blocks_after_max_and_resets() {
        let l = RateLimiter::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            assert!(l.check("k").is_ok());
            l.fail("k");
        }
        assert!(l.check("k").is_err());
        assert!(l.check("other").is_ok());
        l.reset("k");
        assert!(l.check("k").is_ok());
    }

    #[test]
    fn cookies() {
        assert_eq!(
            cookie_session("a=b; orbit_session=abc; c=d").as_deref(),
            Some("abc")
        );
        assert_eq!(
            cookie_session("__Host-orbit_session=zz").as_deref(),
            Some("zz")
        );
        assert_eq!(cookie_session("orbit_session=").as_deref(), None);
        let c = session_cookie(Some("v"), true);
        assert!(c.starts_with("__Host-orbit_session=v;"));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.contains("Secure"));
        assert!(!session_cookie(Some("v"), false).contains("Secure"));
        assert!(session_cookie(None, false).contains("Max-Age=0"));
    }

    #[test]
    fn tokens_have_prefix_and_hash() {
        let (secret, prefix, hash) = new_api_token();
        assert!(secret.starts_with("oc_") && secret.len() > 40);
        assert!(secret.starts_with(&prefix));
        assert_eq!(hash, sha256_hex(&secret));
    }

    #[test]
    fn origin_host() {
        assert_eq!(host_of_origin("https://console.x.test"), "console.x.test");
        assert_eq!(host_of_origin("http://localhost:8080/"), "localhost:8080");
    }
}
