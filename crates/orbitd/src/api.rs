//! The HTTP API (`/v1`, bearer-token protected), `/healthz`, `/metrics` and the web console at `/`. Reference:
//! `docs/api.md`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use oc_store::{LogQuery, StoreError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::executor::RunRequest;
use crate::node::Node;
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
    /// The admin token; `None` disables authentication (development only).
    pub token: Option<Arc<str>>,
}

/// An API error: a status and a message, rendered as `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let code = match e {
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            StoreError::Conflict(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(code, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<StoreError>() {
            Ok(s) => s.into(),
            Err(e) => Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

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
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_MODULE_BYTES));
    Router::new()
        .route("/", get(console))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(crate::metrics::handler))
        .nest("/v1", v1)
        .with_state(state)
}

/// Checks `Authorization: Bearer <token>` (or `?token=` for the browser's EventSource, which cannot set headers).
async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let Some(token) = s.token.as_deref() else {
        return next.run(req).await;
    };
    let header_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()));
    let query_ok = req.uri().query().is_some_and(|q| {
        q.split('&')
            .filter_map(|kv| kv.strip_prefix("token="))
            .any(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
    });
    if header_ok || query_ok {
        next.run(req).await
    } else {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn console() -> Html<&'static str> {
    Html(include_str!("console.html"))
}

// ------------------------------------------------------------------------------------------------ apps

async fn list_apps(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let store = s.node.executor.store();
    let workers = s.node.workers().await;
    let mut out = Vec::new();
    for a in store.apps()? {
        let d = a
            .active_deployment
            .map(|id| store.deployment(id))
            .transpose()?;
        let running = workers.iter().any(|(n, _)| n == &a.name);
        out.push(json!({
            "name": a.name, "created_at": a.created_at, "active_deployment": a.active_deployment,
            "version": d.as_ref().map(|d| d.version), "kind": d.as_ref().and_then(|d| d.spec.get("kind").cloned()).unwrap_or(json!("job")),
            "worker_running": running,
        }));
    }
    Ok(Json(json!(out)))
}

async fn get_app(State(s): State<AppState>, Path(app): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.node.executor.store();
    let a = store.app(&app)?;
    let deployments = store.deployments(&app)?;
    let running = s
        .node
        .workers()
        .await
        .into_iter()
        .find(|(n, _)| n == &app)
        .map(|(_, d)| d);
    Ok(Json(
        json!({ "app": a, "deployments": deployments, "worker_deployment": running }),
    ))
}

async fn delete_app(State(s): State<AppState>, Path(app): Path<String>) -> ApiResult<StatusCode> {
    s.node.executor.store().delete_app(&app)?;
    s.node.reconcile(&app).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_deployments(
    State(s): State<AppState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    s.node.executor.store().app(&app)?;
    Ok(Json(json!(s.node.executor.store().deployments(&app)?)))
}

/// `POST /v1/apps/{app}/deployments`: the body is the module, the spec is the `x-orbit-spec` header (JSON).
async fn deploy(
    State(s): State<AppState>,
    Path(app): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
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
    if !oc_store::valid_name(&app) {
        return Err(ApiError::bad(format!(
            "invalid app name `{app}`: 1-63 of a-z, 0-9 and -"
        )));
    }
    let store = s.node.executor.store();
    let sha = store.put_module(&body)?;
    let d = store.deploy(
        &app,
        &sha,
        &serde_json::to_value(&spec).map_err(|e| ApiError::bad(e.to_string()))?,
    )?;
    s.node.reconcile(&app).await?;
    Ok((StatusCode::CREATED, Json(json!(d))))
}

async fn activate(
    State(s): State<AppState>,
    Path((app, id)): Path<(String, i64)>,
) -> ApiResult<Json<Value>> {
    s.node.executor.store().activate(&app, id)?;
    s.node.reconcile(&app).await?;
    Ok(Json(json!(s.node.executor.store().app(&app)?)))
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
    Path(app): Path<String>,
    Query(p): Query<RunParams>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "run input over 1 MiB".into(),
        ));
    }
    let store = s.node.executor.store();
    let a = store.app(&app)?;
    let id = a
        .active_deployment
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, format!("`{app}` has no deployment")))?;
    let d = store.deployment(id)?;
    if d.spec.get("kind").and_then(Value::as_str) == Some("service") {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "a service runs by itself; deploy it as a job to run it on demand".into(),
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
    if p.detach == Some(true) {
        let run = store.start_run(&app, d.id, "api", None)?;
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
    Path(app): Path<String>,
    Query(l): Query<Limit>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        s.node
            .executor
            .store()
            .runs(Some(&app), l.limit.unwrap_or(50))?
    )))
}

async fn all_runs(State(s): State<AppState>, Query(l): Query<Limit>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        s.node.executor.store().runs(None, l.limit.unwrap_or(50))?
    )))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.node.executor.store().run(&id)?)))
}

async fn get_trace(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.node.executor.store();
    let run = store.run(&id)?;
    Ok(Json(
        json!({ "trace_id": run.trace_id, "spans": store.trace(&run.trace_id)? }),
    ))
}

async fn run_logs(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(mut q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    s.node.executor.store().run(&id)?;
    q.run = Some(id);
    Ok(Json(json!(s.node.executor.store().logs(&q)?)))
}

// ------------------------------------------------------------------------------------------------ logs

async fn logs(State(s): State<AppState>, Query(q): Query<LogQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.node.executor.store().logs(&q)?)))
}

/// Server-Sent Events: every new line matching the query, as `data: {json}`; polls the store every 250 ms.
async fn log_stream(
    State(s): State<AppState>,
    Query(mut q): Query<LogQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let store = Arc::clone(s.node.executor.store());
    if q.after.is_none() {
        // start at the newest line
        let newest = store
            .logs(&LogQuery {
                limit: Some(1),
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
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ------------------------------------------------------------------------------------------------ kv

async fn kv_namespaces(
    State(s): State<AppState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let ns = s.node.executor.store().kv_namespaces(&app)?;
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
    Path((app, ns)): Path<(String, String)>,
    Query(p): Query<KvList>,
) -> ApiResult<Json<Value>> {
    let prefix = p.prefix.unwrap_or_default();
    let entries =
        s.node
            .executor
            .store()
            .kv_list(&app, &ns, prefix.as_bytes(), p.limit.unwrap_or(100))?;
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
    Path((app, ns, key)): Path<(String, String, String)>,
) -> ApiResult<Vec<u8>> {
    s.node
        .executor
        .store()
        .kv_get(&app, &ns, key.as_bytes())?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("key `{key}` not found")))
}

async fn kv_put(
    State(s): State<AppState>,
    Path((app, ns, key)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    s.node.executor.store().app(&app)?;
    s.node
        .executor
        .store()
        .kv_put(&app, &ns, key.as_bytes(), &body)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn kv_delete(
    State(s): State<AppState>,
    Path((app, ns, key)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    if s.node
        .executor
        .store()
        .kv_delete(&app, &ns, key.as_bytes())?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("key `{key}` not found"),
        ))
    }
}

// ------------------------------------------------------------------------------------------------ queues

async fn list_queues(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.node.executor.store().queues()?)))
}

async fn enqueue(
    State(s): State<AppState>,
    Path(q): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "message over 1 MiB".into(),
        ));
    }
    let id = s.node.executor.store().enqueue(&q, &body)?;
    s.node.queue_signal.notify_waiters();
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "queue": q }))))
}

#[derive(Deserialize, Default)]
struct LeaseParams {
    lease_ms: Option<i64>,
}

fn message_json(m: &oc_store::Message) -> Value {
    json!({
        "id": m.id, "queue": m.queue, "body": String::from_utf8_lossy(&m.body), "attempts": m.attempts,
        "enqueued_at": m.enqueued_at, "lease_token": m.lease_token, "last_error": m.last_error,
    })
}

/// Leases a message for an external consumer; 204 when the queue is empty.
async fn lease(
    State(s): State<AppState>,
    Path(q): Path<String>,
    Query(p): Query<LeaseParams>,
) -> ApiResult<Response> {
    match s
        .node
        .executor
        .store()
        .lease(&q, p.lease_ms.unwrap_or(30_000).clamp(1, 3_600_000))?
    {
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
    Path((_q, id)): Path<(String, i64)>,
    Json(b): Json<AckBody>,
) -> ApiResult<StatusCode> {
    s.node.executor.store().ack(id, &b.token)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn nack(
    State(s): State<AppState>,
    Path((_q, id)): Path<(String, i64)>,
    Json(b): Json<AckBody>,
) -> ApiResult<Json<Value>> {
    let o = s.node.executor.store().nack(
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
    Path(q): Path<String>,
    Query(l): Query<Limit>,
) -> ApiResult<Json<Value>> {
    let dead = s
        .node
        .executor
        .store()
        .dead_letters(&q, l.limit.unwrap_or(100))?;
    Ok(Json(json!(
        dead.iter().map(message_json).collect::<Vec<_>>()
    )))
}

async fn redrive(State(s): State<AppState>, Path(q): Path<String>) -> ApiResult<Json<Value>> {
    let n = s.node.executor.store().redrive(&q)?;
    s.node.queue_signal.notify_waiters();
    Ok(Json(json!({ "redriven": n })))
}

// ------------------------------------------------------------------------------------------------ node

async fn node_info(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let workers = s.node.workers().await;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "orbit_abi": orbit_abi::ABI_VERSION,
        "workers": workers.into_iter().map(|(a, d)| json!({ "app": a, "deployment": d })).collect::<Vec<_>>(),
        "kinds": [Kind::Job, Kind::Service, Kind::Consumer],
    })))
}
