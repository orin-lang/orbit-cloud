//! The HTTP API (`/v1`, bearer-token protected), `/healthz`, `/metrics` and the web console at `/`. Reference:
//! `docs/api.md`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use oc_store::{LogQuery, StoreError, key_of, valid_name};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::auth::{Authz, Perm, RequestMeta};
use crate::executor::RunRequest;
use crate::node::Node;
use crate::platform::{self, Platform};
use crate::spec::{Kind, Spec};

/// Largest accepted module, 64 MiB.
pub const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;
/// Largest queue message or run input, 1 MiB.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Shared state of the handlers.
#[derive(Clone, Debug)]
pub struct AppState {
    /// The node.
    pub node: Arc<Node>,
    /// The break-glass admin token; with none, and no user yet, the API is open (development only).
    pub token: Option<Arc<str>>,
    /// Users, hosts, setup.
    pub platform: Arc<Platform>,
}

impl AppState {
    /// State with a default platform (no base domain, real DNS).
    #[must_use]
    pub fn new(node: Arc<Node>, token: Option<Arc<str>>) -> Self {
        Self {
            node,
            token,
            platform: Arc::new(Platform::default()),
        }
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &Arc<oc_store::Store> {
        self.node.executor.store()
    }
}

/// An API error: a status and a message, rendered as `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError {
    /// Status code.
    pub status: StatusCode,
    /// Message for the caller.
    pub message: String,
    headers: Vec<(&'static str, String)>,
}

impl ApiError {
    /// An error with a status.
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            headers: Vec::new(),
        }
    }

    /// An error with one extra response header.
    pub fn with_header(
        status: StatusCode,
        message: impl Into<String>,
        name: &'static str,
        value: String,
    ) -> Self {
        Self {
            headers: vec![(name, value)],
            ..Self::new(status, message)
        }
    }

    /// 400.
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.status, Json(json!({ "error": self.message }))).into_response();
        for (k, v) in self.headers {
            if let Ok(v) = HeaderValue::from_str(&v) {
                r.headers_mut().insert(HeaderName::from_static(k), v);
            }
        }
        r
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let code = match e {
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            StoreError::Conflict(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(code, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<StoreError>() {
            Ok(s) => s.into(),
            Err(e) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// The project a request is about: set by the `/v1/projects/{p}/...` rewrite, else `?project=`, else `default`.
#[derive(Clone, Debug)]
pub struct Scope(pub String);

/// Extension set by the path rewrite.
#[derive(Clone, Debug)]
struct ProjectScope(String);

impl<S: Send + Sync> FromRequestParts<S> for Scope {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        let p = if let Some(ProjectScope(p)) = parts.extensions.get::<ProjectScope>() {
            p.clone()
        } else {
            parts
                .uri
                .query()
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("project=")))
                .map_or_else(|| oc_store::DEFAULT_PROJECT.to_owned(), str::to_owned)
        };
        if !valid_name(&p) {
            return Err(ApiError::bad(format!("invalid project name `{p}`")));
        }
        Ok(Scope(p))
    }
}

impl Scope {
    /// The key of app `name` in this project.
    fn app(&self, name: &str) -> ApiResult<String> {
        if !valid_name(name) {
            return Err(ApiError::bad(format!(
                "invalid app name `{name}`: 1-63 of a-z, 0-9 and -"
            )));
        }
        Ok(key_of(&self.0, name))
    }

    /// The key of queue `name` in this project.
    fn queue(&self, name: &str) -> ApiResult<String> {
        if !valid_name(name) {
            return Err(ApiError::bad(format!("invalid queue name `{name}`")));
        }
        Ok(key_of(&self.0, name))
    }
}

/// Path segments below `/v1/projects/{p}/` that are the project-scoped twins of the `/v1/...` resources.
const SCOPED: [&str; 4] = ["apps", "runs", "logs", "queues"];

/// `/v1/projects/{p}/apps/x?y` -> (`p`, `/v1/apps/x?y`).
fn rewrite(uri: &Uri) -> Option<(String, Uri)> {
    let rest = uri.path().strip_prefix("/v1/projects/")?;
    let (project, tail) = rest.split_once('/')?;
    let first = tail.split('/').next()?;
    if !SCOPED.contains(&first) {
        return None;
    }
    let pq = match uri.query() {
        Some(q) => format!("/v1/{tail}?{q}"),
        None => format!("/v1/{tail}"),
    };
    Some((project.to_owned(), pq.parse().ok()?))
}

async fn dispatch(inner: Router, mut req: Request) -> Response {
    if let Some((project, uri)) = rewrite(req.uri()) {
        *req.uri_mut() = uri;
        req.extensions_mut().insert(ProjectScope(project));
    }
    match inner.oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    }
}

/// Fills in [`RequestMeta`] from the TCP peer when the gateway did not.
async fn meta_layer(mut req: Request, next: Next) -> Response {
    if req.extensions().get::<RequestMeta>().is_none() {
        let ip = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        req.extensions_mut()
            .insert(RequestMeta { ip, https: false });
    }
    next.run(req).await
}

/// The router.
pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/apps", get(list_apps))
        .route("/apps/{app}", get(get_app).delete(delete_app))
        .route(
            "/apps/{app}/deployments",
            get(list_deployments).post(deploy),
        )
        .route("/apps/{app}/deployments/{id}/activate", post(activate))
        .route("/apps/{app}/runs", get(list_runs).post(start_run))
        .route("/apps/{app}/kv", get(kv_namespaces))
        .route("/apps/{app}/kv/{ns}", get(kv_list))
        .route(
            "/apps/{app}/kv/{ns}/{key}",
            get(kv_get).put(kv_put).delete(kv_delete),
        )
        .route("/runs", get(all_runs))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/trace", get(get_trace))
        .route("/runs/{id}/logs", get(run_logs))
        .route("/logs", get(logs))
        .route("/logs/stream", get(log_stream))
        .route("/queues", get(list_queues))
        .route("/queues/{q}/messages", post(enqueue))
        .route("/queues/{q}/lease", post(lease))
        .route("/queues/{q}/messages/{id}/ack", post(ack))
        .route("/queues/{q}/messages/{id}/nack", post(nack))
        .route("/queues/{q}/dead", get(dead_letters))
        .route("/queues/{q}/redrive", post(redrive))
        .route("/node", get(node_info))
        .merge(platform::routes())
        .layer(axum::extract::DefaultBodyLimit::max(MAX_MODULE_BYTES));
    let inner = Router::new()
        .route("/", get(console))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(crate::metrics::handler))
        .route("/setup", get(platform::setup_page))
        .nest("/v1", v1)
        .with_state(state);
    Router::new()
        .fallback(move |req: Request| {
            let inner = inner.clone();
            async move { dispatch(inner, req).await }
        })
        .layer(middleware::from_fn(meta_layer))
}

async fn console() -> Html<&'static str> {
    Html(include_str!("console.html"))
}

// ------------------------------------------------------------------------------------------------ apps

/// The public shape of an app: `name` is the name inside the project, `key` the engine-wide key.
fn app_json(a: &oc_store::App) -> Value {
    json!({
        "name": a.short, "project": a.project, "key": a.name, "created_at": a.created_at,
        "active_deployment": a.active_deployment,
    })
}

async fn list_apps(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let store = s.store();
    let workers = s.node.workers().await;
    let mut out = Vec::new();
    for a in store.apps_in(&scope.0)? {
        let d = a
            .active_deployment
            .map(|id| store.deployment(id))
            .transpose()?;
        let running = workers.iter().any(|(n, _)| n == &a.name);
        let mut v = app_json(&a);
        v["version"] = json!(d.as_ref().map(|d| d.version));
        v["kind"] = d
            .as_ref()
            .and_then(|d| d.spec.get("kind").cloned())
            .unwrap_or(json!("job"));
        v["http"] = json!(
            d.as_ref()
                .and_then(|d| d.spec.get("http").cloned())
                .unwrap_or(json!(false))
        );
        v["worker_running"] = json!(running);
        out.push(v);
    }
    Ok(Json(json!(out)))
}

async fn get_app(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let key = scope.app(&app)?;
    let store = s.store();
    let a = store.app(&key)?;
    let deployments = store.deployments(&key)?;
    let running = s
        .node
        .workers()
        .await
        .into_iter()
        .find(|(n, _)| n == &key)
        .map(|(_, d)| d);
    Ok(Json(
        json!({ "app": app_json(&a), "deployments": deployments, "worker_deployment": running }),
    ))
}

async fn delete_app(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
) -> ApiResult<StatusCode> {
    let project = authz.require_project(&scope.0, Perm::Write)?;
    let key = scope.app(&app)?;
    remove_app(&s, &key).await?;
    authz.audit(
        "app.delete",
        Some(&key),
        Some(&project.org_id),
        Some(&project.id),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Stops the worker of an app and deletes it with its data and files.
pub(crate) async fn remove_app(s: &AppState, key: &str) -> ApiResult<()> {
    s.store().delete_app(key)?;
    s.node.reconcile(key).await?;
    // after the worker has stopped, so nothing is writing there any more
    s.node
        .executor
        .remove_app_files(key)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("app files: {e}")))?;
    Ok(())
}

async fn list_deployments(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let key = scope.app(&app)?;
    s.store().app(&key)?;
    Ok(Json(json!(s.store().deployments(&key)?)))
}

/// `POST /v1/apps/{app}/deployments`: the body is the module, the spec is the `x-orbit-spec` header (JSON).
async fn deploy(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let project = authz.require_project(&scope.0, Perm::Write)?;
    let key = scope.app(&app)?;
    let spec_text = headers.get("x-orbit-spec").map(|v| {
        v.to_str()
            .map_err(|_| ApiError::bad("x-orbit-spec is not text"))
    });
    let spec_value: Value = match spec_text.transpose()? {
        Some(t) => {
            serde_json::from_str(t).map_err(|e| ApiError::bad(format!("x-orbit-spec: {e}")))?
        }
        None => json!({}),
    };
    let spec: Spec = serde_json::from_value(spec_value.clone())
        .map_err(|e| ApiError::bad(format!("spec: {e}")))?;
    spec.validate().map_err(ApiError::bad)?;
    if body.len() < 8 || &body[..4] != b"\0asm" {
        return Err(ApiError::bad(
            "the body must be a WebAssembly module (\\0asm)",
        ));
    }
    let store = s.store();
    let sha = store.put_module(&body)?;
    let d = store.deploy(
        &key,
        &sha,
        &serde_json::to_value(&spec).map_err(|e| ApiError::bad(e.to_string()))?,
    )?;
    if spec.http {
        // the port is stable per app; the first HTTP app of a project gets the `/` route
        store.http_port(&key)?;
        if !store
            .routes(&project.name)?
            .iter()
            .any(|r| r.host.is_empty() && r.path_prefix == "/")
        {
            store.add_route(&project.name, "", "/", &key)?;
        }
    }
    s.node.reconcile(&key).await?;
    authz.audit(
        "app.deploy",
        Some(&key),
        Some(&project.org_id),
        Some(&project.id),
        Some(json!({ "version": d.version, "module": d.module, "kind": spec.kind })),
    );
    Ok((StatusCode::CREATED, Json(json!(d))))
}

async fn activate(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((app, id)): Path<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let project = authz.require_project(&scope.0, Perm::Write)?;
    let key = scope.app(&app)?;
    s.store().activate(&key, id)?;
    s.node.reconcile(&key).await?;
    authz.audit(
        "app.activate",
        Some(&key),
        Some(&project.org_id),
        Some(&project.id),
        Some(json!({ "deployment": id })),
    );
    Ok(Json(app_json(&s.store().app(&key)?)))
}

// ------------------------------------------------------------------------------------------------ runs

#[derive(Deserialize, Default)]
struct RunParams {
    /// Integer arguments of `entry`, comma-separated.
    args: Option<String>,
    /// Return at once with the running record instead of waiting for the end.
    detach: Option<bool>,
}

/// `POST /v1/apps/{app}/runs`: runs the active deployment once; the body becomes the run's stdin.
async fn start_run(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
    Query(p): Query<RunParams>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let project = authz.require_project(&scope.0, Perm::Write)?;
    let key = scope.app(&app)?;
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "run input over 1 MiB",
        ));
    }
    let store = s.store();
    let a = store.app(&key)?;
    let id = a
        .active_deployment
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, format!("`{app}` has no deployment")))?;
    let d = store.deployment(id)?;
    if d.spec.get("kind").and_then(Value::as_str) == Some("service") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a service runs by itself; deploy it as a job to run it on demand",
        ));
    }
    let args = match p.args.as_deref().filter(|a| !a.is_empty()) {
        Some(a) => a
            .split(',')
            .map(|x| {
                x.trim()
                    .parse::<i64>()
                    .map_err(|_| ApiError::bad(format!("bad integer argument `{x}`")))
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let req = RunRequest {
        trigger: "api".into(),
        stdin: (!body.is_empty()).then(|| body.to_vec()),
        args,
        message_id: None,
    };
    authz.audit(
        "app.run",
        Some(&key),
        Some(&project.org_id),
        Some(&project.id),
        None,
    );
    if p.detach == Some(true) {
        let run = store.start_run(&key, d.id, "api", None)?;
        let exec = Arc::clone(&s.node.executor);
        let r2 = run.clone();
        tokio::spawn(async move {
            if let Err(e) = exec.execute(&d, r2, req).await {
                tracing::error!("detached run failed to record: {e}");
            }
        });
        return Ok((StatusCode::ACCEPTED, Json(json!(run))));
    }
    let run = s.node.executor.run(&d, req).await?;
    Ok((StatusCode::OK, Json(json!(run))))
}

#[derive(Deserialize, Default)]
struct Limit {
    limit: Option<i64>,
}

async fn list_runs(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
    Query(l): Query<Limit>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let key = scope.app(&app)?;
    Ok(Json(json!(
        s.store().runs(Some(&key), l.limit.unwrap_or(50))?
    )))
}

async fn all_runs(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Query(l): Query<Limit>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    Ok(Json(json!(
        s.store().runs_in(&scope.0, l.limit.unwrap_or(50))?
    )))
}

async fn get_run(
    State(s): State<AppState>,
    authz: Authz,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let run = s.store().run(&id)?;
    authz.require_app_key(&run.app, Perm::Read)?;
    Ok(Json(json!(run)))
}

async fn get_trace(
    State(s): State<AppState>,
    authz: Authz,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let store = s.store();
    let run = store.run(&id)?;
    authz.require_app_key(&run.app, Perm::Read)?;
    Ok(Json(
        json!({ "trace_id": run.trace_id, "spans": store.trace(&run.trace_id)? }),
    ))
}

async fn run_logs(
    State(s): State<AppState>,
    authz: Authz,
    Path(id): Path<String>,
    Query(mut q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    let run = s.store().run(&id)?;
    authz.require_app_key(&run.app, Perm::Read)?;
    q.run = Some(id);
    q.app = None;
    q.project = None;
    Ok(Json(json!(s.store().logs(&q)?)))
}

// ------------------------------------------------------------------------------------------------ logs

/// Narrows a client's log query to the project of the request (the `app` filter is a name inside it).
fn scope_logs(scope: &Scope, q: &mut LogQuery) -> ApiResult<()> {
    q.app = q.app.as_deref().map(|a| scope.app(a)).transpose()?;
    q.project = Some(scope.0.clone());
    Ok(())
}

async fn logs(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Query(mut q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    scope_logs(&scope, &mut q)?;
    Ok(Json(json!(s.store().logs(&q)?)))
}

/// Server-Sent Events: every new line matching the query, as `data: {json}`; polls the store every 250 ms.
async fn log_stream(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Query(mut q): Query<LogQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    authz.require_project(&scope.0, Perm::Read)?;
    scope_logs(&scope, &mut q)?;
    let store = Arc::clone(s.store());
    if q.after.is_none() {
        // start at the newest line
        let newest = store
            .logs(&LogQuery {
                limit: Some(1),
                project: q.project.clone(),
                ..LogQuery::default()
            })
            .ok()
            .and_then(|v| v.last().map(|l| l.id));
        q.after = Some(newest.unwrap_or(0));
    }
    let stream = futures_util::stream::unfold(
        (store, q, Vec::new()),
        |(store, mut q, mut pending)| async move {
            loop {
                if let Some(line) = pending.pop() {
                    let ev = Event::default()
                        .json_data(&line)
                        .unwrap_or_else(|_| Event::default());
                    return Some((Ok(ev), (store, q, pending)));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                if let Ok(mut lines) = store.logs(&LogQuery {
                    limit: Some(500),
                    ..q.clone()
                }) {
                    if let Some(last) = lines.last() {
                        q.after = Some(last.id);
                    }
                    lines.reverse();
                    pending = lines;
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ------------------------------------------------------------------------------------------------ kv

async fn kv_namespaces(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let ns = s.store().kv_namespaces(&scope.app(&app)?)?;
    Ok(Json(json!(
        ns.into_iter()
            .map(|(n, c)| json!({ "namespace": n, "entries": c }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize, Default)]
struct KvList {
    prefix: Option<String>,
    limit: Option<i64>,
}

async fn kv_list(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((app, ns)): Path<(String, String)>,
    Query(p): Query<KvList>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let prefix = p.prefix.unwrap_or_default();
    let entries = s.store().kv_list(
        &scope.app(&app)?,
        &ns,
        prefix.as_bytes(),
        p.limit.unwrap_or(100),
    )?;
    Ok(Json(json!(
        entries
            .into_iter()
            .map(|e| json!({
                "key": String::from_utf8_lossy(&e.key), "value": String::from_utf8_lossy(&e.value),
                "size": e.value.len(), "updated_at": e.updated_at,
            }))
            .collect::<Vec<_>>()
    )))
}

async fn kv_get(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((app, ns, key)): Path<(String, String, String)>,
) -> ApiResult<Vec<u8>> {
    authz.require_project(&scope.0, Perm::Read)?;
    s.store()
        .kv_get(&scope.app(&app)?, &ns, key.as_bytes())?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("key `{key}` not found")))
}

async fn kv_put(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((app, ns, key)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    authz.require_project(&scope.0, Perm::Write)?;
    let k = scope.app(&app)?;
    s.store().app(&k)?;
    s.store().kv_put(&k, &ns, key.as_bytes(), &body)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn kv_delete(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((app, ns, key)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    authz.require_project(&scope.0, Perm::Write)?;
    if s.store()
        .kv_delete(&scope.app(&app)?, &ns, key.as_bytes())?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("key `{key}` not found"),
        ))
    }
}

// ------------------------------------------------------------------------------------------------ queues

/// The queues of one project, named without the project prefix.
async fn list_queues(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let mut out = Vec::new();
    for mut q in s.store().queues()? {
        let (p, name) = oc_store::split_key(&q.queue);
        if p == scope.0 {
            q.queue = name.to_owned();
            out.push(q);
        }
    }
    Ok(Json(json!(out)))
}

async fn enqueue(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(q): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authz.require_project(&scope.0, Perm::Write)?;
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "message over 1 MiB",
        ));
    }
    let id = s.store().enqueue(&scope.queue(&q)?, &body)?;
    s.node.queue_signal.notify_waiters();
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "queue": q }))))
}

#[derive(Deserialize, Default)]
struct LeaseParams {
    lease_ms: Option<i64>,
}

fn message_json(m: &oc_store::Message) -> Value {
    json!({
        "id": m.id, "queue": oc_store::split_key(&m.queue).1, "body": String::from_utf8_lossy(&m.body), "attempts": m.attempts,
        "enqueued_at": m.enqueued_at, "lease_token": m.lease_token, "last_error": m.last_error,
    })
}

/// Leases a message for an external consumer; 204 when the queue is empty.
async fn lease(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(q): Path<String>,
    Query(p): Query<LeaseParams>,
) -> ApiResult<Response> {
    authz.require_project(&scope.0, Perm::Write)?;
    match s.store().lease(
        &scope.queue(&q)?,
        p.lease_ms.unwrap_or(30_000).clamp(1, 3_600_000),
    )? {
        Some(m) => Ok(Json(message_json(&m)).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[derive(Deserialize)]
struct AckBody {
    token: String,
    error: Option<String>,
    delay_ms: Option<i64>,
    max_attempts: Option<i64>,
}

async fn ack(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((_q, id)): Path<(String, i64)>,
    Json(b): Json<AckBody>,
) -> ApiResult<StatusCode> {
    authz.require_project(&scope.0, Perm::Write)?;
    s.store().ack(id, &b.token)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn nack(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path((_q, id)): Path<(String, i64)>,
    Json(b): Json<AckBody>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Write)?;
    let o = s.store().nack(
        id,
        &b.token,
        b.error.as_deref().unwrap_or("rejected"),
        b.delay_ms.unwrap_or(0),
        b.max_attempts.unwrap_or(5),
    )?;
    Ok(Json(json!({ "outcome": o })))
}

async fn dead_letters(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(q): Path<String>,
    Query(l): Query<Limit>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let dead = s
        .store()
        .dead_letters(&scope.queue(&q)?, l.limit.unwrap_or(100))?;
    Ok(Json(json!(
        dead.iter().map(message_json).collect::<Vec<_>>()
    )))
}

async fn redrive(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
    Path(q): Path<String>,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Write)?;
    let n = s.store().redrive(&scope.queue(&q)?)?;
    s.node.queue_signal.notify_waiters();
    Ok(Json(json!({ "redriven": n })))
}

// ------------------------------------------------------------------------------------------------ node

async fn node_info(
    State(s): State<AppState>,
    authz: Authz,
    scope: Scope,
) -> ApiResult<Json<Value>> {
    authz.require_project(&scope.0, Perm::Read)?;
    let workers = s.node.workers().await;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "orbit_abi": orbit_abi::ABI_VERSION,
        "base_domain": s.platform.base_domain,
        "workers": workers.into_iter()
            .filter(|(a, _)| oc_store::split_key(a).0 == scope.0)
            .map(|(a, d)| json!({ "app": oc_store::split_key(&a).1, "deployment": d }))
            .collect::<Vec<_>>(),
        "kinds": [Kind::Job, Kind::Service, Kind::Consumer],
    })))
}
