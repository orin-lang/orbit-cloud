//! End to end through the HTTP API, with real WebAssembly guests on the Orbit runtime.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::node::Node;
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "test-token";

struct Fx {
    _dir: tempfile::TempDir,
    app: Router,
    node: Arc<Node>,
}

async fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let exec = Arc::new(Executor::new(store, dir.path(), true).unwrap());
    let node = Node::new(exec);
    let app = router(AppState {
        node: Arc::clone(&node),
        token: Some(Arc::from(TOKEN)),
    });
    Fx {
        _dir: dir,
        app,
        node,
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    spec: Option<&Value>,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"));
    if let Some(s) = spec {
        b = b.header("x-orbit-spec", s.to_string());
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

async fn json_call(app: &Router, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let (s, b) = call(app, method, uri, None, body).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn deploy(app: &Router, name: &str, wat_src: &str, spec: Value) -> Value {
    let wasm = wat::parse_str(wat_src).unwrap();
    let (s, b) = call(
        app,
        "POST",
        &format!("/v1/apps/{name}/deployments"),
        Some(&spec),
        wasm,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "deploy {name}: {}",
        String::from_utf8_lossy(&b)
    );
    serde_json::from_slice(&b).unwrap()
}

/// Logs that reached the store for a run (the writer batches every 200 ms).
async fn logs_of(app: &Router, run: &str, want: usize) -> Vec<String> {
    for _ in 0..50 {
        let (_, v) = json_call(app, "GET", &format!("/v1/runs/{run}/logs"), vec![]).await;
        let lines: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["message"].as_str().unwrap().to_owned())
            .collect();
        if lines.len() >= want {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("logs of {run} did not arrive");
}

const HELLO: &str = r#"(module
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "hello from orbit cloud")
  (func (export "_start") (drop (call $log (i32.const 2) (i64.const 16) (i64.const 22)))))"#;

/// Increments the 8-byte counter `n` in KV namespace `state` and logs "counted".
const COUNTER: &str = r#"(module
  (import "orbit:kv/v1" "open" (func $open (param i64 i64) (result i64)))
  (import "orbit:kv/v1" "get" (func $get (param i64 i64 i64 i64 i64) (result i64)))
  (import "orbit:kv/v1" "put" (func $put (param i64 i64 i64 i64 i64) (result i32)))
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "state")
  (data (i32.const 8) "n")
  (data (i32.const 32) "counted")
  (func (export "_start") (local $h i64)
    (local.set $h (call $open (i64.const 0) (i64.const 5)))
    (i64.store (i32.const 16) (i64.const 0))
    (drop (call $get (local.get $h) (i64.const 8) (i64.const 1) (i64.const 16) (i64.const 8)))
    (i64.store (i32.const 16) (i64.add (i64.load (i32.const 16)) (i64.const 1)))
    (drop (call $put (local.get $h) (i64.const 8) (i64.const 1) (i64.const 16) (i64.const 8)))
    (drop (call $log (i32.const 2) (i64.const 32) (i64.const 7)))))"#;

/// Logs its stdin; traps when the message is "fail".
const CONSUMER: &str = r#"(module
  (import "orbit:process/v1" "stdin_read" (func $read (param i64 i64 i64) (result i64)))
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start") (local $n i64)
    (local.set $n (call $read (i64.const 100) (i64.const 64) (i64.const 0)))
    (drop (call $log (i32.const 2) (i64.const 100) (local.get $n)))
    (if (i32.eq (i32.load (i32.const 100)) (i32.const 0x6c696166)) ;; "fail"
      (then unreachable))))"#;

/// Publishes "hello" to queue `jobs` and logs the status code digit ('0' = OK, otherwise '-').
const PUBLISHER: &str = r#"(module
  (import "orbit:queue/v1" "publish" (func $pub (param i64 i64 i64 i64) (result i32)))
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "jobs")
  (data (i32.const 8) "hello")
  (data (i32.const 32) "0-")
  (func (export "_start") (local $st i32)
    (local.set $st (call $pub (i64.const 0) (i64.const 4) (i64.const 8) (i64.const 5)))
    (drop (call $log (i32.const 2)
      (select (i64.const 32) (i64.const 33) (i32.eqz (local.get $st))) (i64.const 1)))
    (if (local.get $st) (then unreachable))))"#;

/// An Orin-style program whose `orin_main` returns exit status 3.
const EXIT3: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "orin_main") (result i64) (i64.const 3)))"#;

/// A service that never ends on its own.
const SPIN: &str = r#"(module
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "up")
  (func (export "_start")
    (drop (call $log (i32.const 2) (i64.const 0) (i64.const 2)))
    (loop $l (br $l))))"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_health_and_metrics() {
    let f = fx().await;
    let resp = f
        .app
        .clone()
        .oneshot(Request::get("/v1/apps").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = f
        .app
        .clone()
        .oneshot(
            Request::get("/v1/apps")
                .header("authorization", "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = f
        .app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (s, apps) = json_call(&f.app, "GET", "/v1/apps", vec![]).await;
    assert_eq!((s, apps), (StatusCode::OK, json!([])));
    let resp = f
        .app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the console is served without a token"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_runs_with_logs_trace_and_metrics() {
    let f = fx().await;
    let d = deploy(&f.app, "hello", HELLO, json!({})).await;
    assert_eq!(d["version"], 1);
    let (s, run) = json_call(&f.app, "POST", "/v1/apps/hello/runs", vec![]).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["exit_code"], 0);
    assert!(
        run["host_calls"]["log/v1"].as_u64().unwrap_or(0) >= 1,
        "{run}"
    );
    let id = run["id"].as_str().unwrap();
    assert_eq!(logs_of(&f.app, id, 1).await, ["hello from orbit cloud"]);
    let (_, trace) = json_call(&f.app, "GET", &format!("/v1/runs/{id}/trace"), vec![]).await;
    let names: Vec<&str> = trace["spans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    for n in ["run", "load", "instantiate", "execute"] {
        assert!(names.contains(&n), "span {n} in {names:?}");
    }
    let (_, filtered) = json_call(&f.app, "GET", "/v1/logs?app=hello&contains=orbit", vec![]).await;
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    let (s, body) = call(&f.app, "GET", "/metrics", None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("orbit_cloud_runs{app=\"hello\",status=\"succeeded\"} 1"),
        "{text}"
    );
    // a bad module and a bad spec are refused
    let (s, _) = call(
        &f.app,
        "POST",
        "/v1/apps/x/deployments",
        None,
        b"not wasm".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &f.app,
        "POST",
        "/v1/apps/x/deployments",
        Some(&json!({"nope": 1})),
        wat::parse_str(HELLO).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_persists_across_runs_and_is_denied_without_grant() {
    let f = fx().await;
    deploy(
        &f.app,
        "counter",
        COUNTER,
        json!({ "kv_namespaces": ["state"] }),
    )
    .await;
    for _ in 0..3 {
        let (_, run) = json_call(&f.app, "POST", "/v1/apps/counter/runs", vec![]).await;
        assert_eq!(run["status"], "succeeded", "{run}");
    }
    let (s, v) = call(&f.app, "GET", "/v1/apps/counter/kv/state/n", None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(i64::from_le_bytes(v.try_into().unwrap()), 3);
    let (_, ns) = json_call(&f.app, "GET", "/v1/apps/counter/kv", vec![]).await;
    assert_eq!(ns, json!([{ "namespace": "state", "entries": 1 }]));
    // Without the grant the import does not link: the run fails, nothing is written.
    deploy(&f.app, "counter", COUNTER, json!({})).await;
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/counter/runs", vec![]).await;
    assert_eq!(run["status"], "failed", "{run}");
    let (_, v) = call(&f.app, "GET", "/v1/apps/counter/kv/state/n", None, vec![]).await;
    assert_eq!(i64::from_le_bytes(v.try_into().unwrap()), 3);
    // rollback to version 1 restores the grant
    let (_, a) = json_call(&f.app, "GET", "/v1/apps/counter", vec![]).await;
    let v1 = a["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["version"] == 1)
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (s, _) = json_call(
        &f.app,
        "POST",
        &format!("/v1/apps/counter/deployments/{v1}/activate"),
        vec![],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/counter/runs", vec![]).await;
    assert_eq!(run["status"], "succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_publishes_to_a_queue_a_consumer_reads_and_is_denied_without_grant() {
    let f = fx().await;
    deploy(
        &f.app,
        "worker",
        CONSUMER,
        json!({ "kind": "consumer", "queue": "jobs" }),
    )
    .await;
    deploy(&f.app, "pub", PUBLISHER, json!({ "publish": ["jobs"] })).await;
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/pub/runs", vec![]).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let mut done = false;
    for _ in 0..200 {
        let (_, q) = json_call(&f.app, "GET", "/v1/queues", vec![]).await;
        if q[0]["done"] == 1 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the consumer did not receive the published message");
    let (_, runs) = json_call(&f.app, "GET", "/v1/apps/worker/runs", vec![]).await;
    let id = runs[0]["id"].as_str().unwrap();
    assert_eq!(logs_of(&f.app, id, 1).await, ["hello"]);
    // Publishing to a queue outside the grant is refused (the guest traps on a non-zero status).
    deploy(&f.app, "pub", PUBLISHER, json!({ "publish": ["other"] })).await;
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/pub/runs", vec![]).await;
    assert_eq!(run["status"], "failed", "{run}");
    // Without the capability the import does not link.
    deploy(&f.app, "pub", PUBLISHER, json!({})).await;
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/pub/runs", vec![]).await;
    assert_eq!(run["status"], "failed", "{run}");
    let (_, q) = json_call(&f.app, "GET", "/v1/queues", vec![]).await;
    assert_eq!(q.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orin_main_exit_status_fails_the_run() {
    let f = fx().await;
    deploy(&f.app, "exit3", EXIT3, json!({})).await;
    let (_, run) = json_call(&f.app, "POST", "/v1/apps/exit3/runs", vec![]).await;
    assert_eq!(
        (run["status"].as_str(), run["exit_code"].as_i64()),
        (Some("failed"), Some(3)),
        "{run}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_acks_retries_and_dead_letters() {
    let f = fx().await;
    deploy(
        &f.app,
        "worker",
        CONSUMER,
        json!({ "kind": "consumer", "queue": "jobs", "max_attempts": 2 }),
    )
    .await;
    let (s, _) = json_call(&f.app, "POST", "/v1/queues/jobs/messages", b"ok-1".to_vec()).await;
    assert_eq!(s, StatusCode::CREATED);
    json_call(&f.app, "POST", "/v1/queues/jobs/messages", b"fail".to_vec()).await;
    let mut stats = Value::Null;
    for _ in 0..200 {
        let (_, q) = json_call(&f.app, "GET", "/v1/queues", vec![]).await;
        stats = q[0].clone();
        if stats["done"] == 1 && stats["dead"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        (stats["done"].as_i64(), stats["dead"].as_i64()),
        (Some(1), Some(1)),
        "{stats}"
    );
    let (_, dead) = json_call(&f.app, "GET", "/v1/queues/jobs/dead", vec![]).await;
    assert_eq!(dead[0]["body"], "fail");
    assert_eq!(dead[0]["attempts"], 2);
    let (_, runs) = json_call(&f.app, "GET", "/v1/apps/worker/runs", vec![]).await;
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 3, "one success, two failed attempts");
    let ok = runs.iter().find(|r| r["status"] == "succeeded").unwrap();
    assert_eq!(
        logs_of(&f.app, ok["id"].as_str().unwrap(), 1).await,
        ["ok-1"]
    );
    assert!(ok["message_id"].is_i64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_is_supervised_and_replaced_on_redeploy() {
    let f = fx().await;
    deploy(&f.app, "svc", SPIN, json!({ "kind": "service" })).await;
    let mut up = false;
    for _ in 0..100 {
        let (_, runs) = json_call(&f.app, "GET", "/v1/apps/svc/runs", vec![]).await;
        if runs
            .as_array()
            .is_some_and(|r| r.len() == 1 && r[0]["status"] == "running")
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the service started");
    assert_eq!(f.node.workers().await.len(), 1);
    let (s, _) = json_call(&f.app, "POST", "/v1/apps/svc/runs", vec![]).await;
    assert_eq!(s, StatusCode::CONFLICT, "a service is not run on demand");
    // Redeploying as a job stops the service; its run is recorded as stopped.
    deploy(&f.app, "svc", HELLO, json!({})).await;
    assert!(f.node.workers().await.is_empty());
    let (_, runs) = json_call(&f.app, "GET", "/v1/apps/svc/runs", vec![]).await;
    let r = &runs[0];
    assert_eq!(r["status"], "failed");
    assert!(r["error"].as_str().unwrap().contains("stopped"), "{r}");
    // Deleting the app removes everything.
    let (s, _) = call(&f.app, "DELETE", "/v1/apps/svc", None, vec![]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = json_call(&f.app, "GET", "/v1/apps/svc", vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_are_stored_when_the_run_reads_as_finished() {
    let f = fx().await;
    deploy(&f.app, "hello", HELLO, json!({})).await;
    for _ in 0..5 {
        let (_, run) = json_call(&f.app, "POST", "/v1/apps/hello/runs", vec![]).await;
        let id = run["id"].as_str().unwrap();
        // one query, no waiting: the 200 ms batch must not leave the lines behind a finished run
        let (_, v) = json_call(&f.app, "GET", &format!("/v1/runs/{id}/logs"), vec![]).await;
        assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wall_clock_limit_fails_a_runaway_job() {
    let f = fx().await;
    deploy(&f.app, "spin", SPIN, json!({"limits": {"timeout_ms": 300}})).await;
    let (s, run) = json_call(&f.app, "POST", "/v1/apps/spin/runs", vec![]).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run["error"].is_string(), "{run}");
}

/// A program built by the real Orin toolchain (`tests/fixtures/hello.or`, see the README there).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orin_real_program_runs() {
    let f = fx().await;
    let wasm = include_bytes!("fixtures/hello.wasm").to_vec();
    let (s, b) = call(
        &f.app,
        "POST",
        "/v1/apps/orin-hello/deployments",
        Some(&json!({})),
        wasm,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let (s, run) = json_call(&f.app, "POST", "/v1/apps/orin-hello/runs", vec![]).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["exit_code"], 0, "{run}");
    let id = run["id"].as_str().unwrap();
    assert_eq!(logs_of(&f.app, id, 1).await, ["hi"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_an_app_removes_its_files_and_kv() {
    let f = fx().await;
    deploy(&f.app, "gone", HELLO, json!({"files": true})).await;
    let dir = f._dir.path().join("files/gone");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("secret.txt"), b"old tenant data").unwrap();
    let (s, _) = call(&f.app, "PUT", "/v1/apps/gone/kv/ns/k", None, b"v".to_vec()).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&f.app, "DELETE", "/v1/apps/gone", None, vec![]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(!dir.exists(), "the files directory outlived the app");
    // an app of the same name starts empty
    deploy(&f.app, "gone", HELLO, json!({"files": true})).await;
    let (_, v) = json_call(&f.app, "GET", "/v1/apps/gone/kv", vec![]).await;
    assert_eq!(v, json!([]));
    assert!(!dir.exists());
}
