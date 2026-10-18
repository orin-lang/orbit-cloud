//! Runs deployments on one Orbit runtime. A run is: load the module (cached by content hash), spawn an isolate with
//! the spec's grants and limits, initialise it, call its entry point, record the outcome. Every step is a span of the
//! run's trace; every log line the guest writes is tagged with the run.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use oc_store::{Deployment, NewLog, Run, RunOutcome, Span, Store, random_id};
use orbit_caps::{
    ClockCapability, FsCapability, KvCapability, KvError, KvStore, LogCapability, LogRecord,
    LogSink, NetCapability, ProcessCapability, QueueCapability, RandomCapability, StdinError,
    StdinProvider, StdinReader, WasiCapability,
};
use orbit_runtime::{CapabilityRegistry, ExecError, InstanceInfo, Isolate, Runtime, RuntimeConfig};
use tokio::sync::{mpsc, oneshot};

use crate::provision::Postgres;
use crate::queues::{GuestQueues, MessageQueue};
use crate::spec::{Injected, Spec};

/// Key in the `process/v1` grant config that names the stdin bytes of a run (ignored by Orbit, read by [`RunStdin`]).
pub const STDIN_KEY: &str = "orbit_cloud_stdin";
const ORIN_MAIN: &str = "orin_main";
const ORIN_EXIT_STATUS: &str = "orin_exit_status";

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

// ------------------------------------------------------------------------------------------------ log capture

/// Which run an isolate belongs to, so its log lines can be tagged.
#[derive(Default)]
struct RunIndex(Mutex<HashMap<u64, String>>);

impl RunIndex {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<u64, String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The `LogSink` of every isolate: tags records with app and run and hands them to the batch writer. Never blocks the
/// guest: the channel is unbounded and the writer drains it in batches.
struct CloudSink {
    runs: Arc<RunIndex>,
    tx: mpsc::UnboundedSender<LogMsg>,
    lines: Arc<AtomicU64>,
}

impl LogSink for CloudSink {
    fn write(&self, r: &LogRecord) {
        let run_id = self.runs.map().get(&r.instance.0).cloned();
        self.lines.fetch_add(1, Ordering::Relaxed);
        // A send fails only after shutdown; the line is dropped then.
        let _ = self.tx.send(LogMsg::Line(NewLog {
            ts: oc_store::now_ms(),
            app: r.tenant.to_string(),
            run_id,
            level: r.level.as_str().to_owned(),
            message: r.message.clone(),
        }));
    }
}

/// What the log channel carries: a line, or a request to acknowledge once everything sent before it is stored.
enum LogMsg {
    Line(NewLog),
    Flush(oneshot::Sender<()>),
}

/// Writes log lines to the store in batches (up to 500 lines or every 200 ms). A flush request ends the batch at
/// once and is acknowledged after the write.
async fn log_writer(store: Arc<Store>, mut rx: mpsc::UnboundedReceiver<LogMsg>) {
    let mut batch = Vec::new();
    let mut acks = Vec::new();
    loop {
        let Some(first) = rx.recv().await else { break };
        let mut open = true;
        match first {
            LogMsg::Line(l) => batch.push(l),
            LogMsg::Flush(a) => {
                acks.push(a);
                open = false;
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
        while open && batch.len() < 500 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(LogMsg::Line(l))) => batch.push(l),
                Ok(Some(LogMsg::Flush(a))) => {
                    acks.push(a);
                    open = false;
                }
                Ok(None) | Err(_) => break,
            }
        }
        let lines = std::mem::take(&mut batch);
        if !lines.is_empty() {
            let s = Arc::clone(&store);
            let res = tokio::task::spawn_blocking(move || s.append_logs(&lines)).await;
            if let Ok(Err(e)) = res {
                tracing::error!("writing log lines failed: {e}");
            }
        }
        for a in acks.drain(..) {
            let _ = a.send(());
        }
    }
}

// ------------------------------------------------------------------------------------------------ kv and stdin

/// `orbit:kv/v1` backed by the store: the data survives restarts and is browsable through the API.
struct StoreKv(Arc<Store>);

impl KvStore for StoreKv {
    fn get(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.0
            .kv_get(tenant, namespace, key)
            .map_err(|e| KvError(e.to_string()))
    }
    fn put(&self, tenant: &str, namespace: &str, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        self.0
            .kv_put(tenant, namespace, key, value)
            .map_err(|e| KvError(e.to_string()))
    }
    fn delete(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<(), KvError> {
        self.0
            .kv_delete(tenant, namespace, key)
            .map(|_| ())
            .map_err(|e| KvError(e.to_string()))
    }
}

/// Stdin of a run (a queue message, or the body of an API request): the bytes are parked under a key that the
/// run's `process/v1` grant carries in [`STDIN_KEY`]; the isolate takes them when it is created.
#[derive(Clone, Default)]
struct RunStdin(Arc<Mutex<HashMap<String, Vec<u8>>>>);

struct BytesReader(std::io::Cursor<Vec<u8>>);

impl StdinReader for BytesReader {
    fn read(
        &mut self,
        buf: &mut [u8],
        _timeout: Option<std::time::Duration>,
    ) -> Result<usize, StdinError> {
        std::io::Read::read(&mut self.0, buf).map_err(|_| StdinError::Io)
    }
}

impl StdinProvider for RunStdin {
    fn open(&self, info: &InstanceInfo) -> Box<dyn StdinReader> {
        let key = info
            .grants
            .config(&orbit_abi::CapabilityId::process_v1())
            .and_then(|c| c.get(STDIN_KEY))
            .and_then(serde_json::Value::as_str);
        let bytes = key
            .and_then(|k| {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(k)
            })
            .unwrap_or_default();
        Box::new(BytesReader(std::io::Cursor::new(bytes)))
    }
}

// ------------------------------------------------------------------------------------------------ the executor

/// Counters for `/metrics` that the store does not keep.
#[derive(Default, Debug)]
pub struct Counters {
    /// Log lines accepted.
    pub log_lines: Arc<AtomicU64>,
    /// Runs started by this process.
    pub runs_started: AtomicU64,
    /// Sum of finished runs' wall time, ms.
    pub run_ms_total: AtomicU64,
    /// Runs currently executing.
    pub runs_active: AtomicU64,
}

/// Runs deployments.
pub struct Executor {
    store: Arc<Store>,
    runtime: Runtime,
    runs: Arc<RunIndex>,
    stdin: RunStdin,
    files_root: PathBuf,
    queues: Arc<dyn MessageQueue>,
    postgres: Option<Arc<Postgres>>,
    queue_signal: Arc<tokio::sync::Notify>,
    log_tx: mpsc::UnboundedSender<LogMsg>,
    /// Metrics.
    pub counters: Arc<Counters>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor").finish_non_exhaustive()
    }
}

/// What to run.
#[derive(Debug, Default)]
pub struct RunRequest {
    /// `api`, `service` or `queue`.
    pub trigger: String,
    /// Bytes on stdin.
    pub stdin: Option<Vec<u8>>,
    /// Arguments of a `--invoke`-style call of `entry` (integers, as Orbit exports take them).
    pub args: Vec<i64>,
    /// The queue message.
    pub message_id: Option<i64>,
}

struct Timeline {
    trace: String,
    run: String,
    root: String,
    spans: Vec<Span>,
}

impl Timeline {
    fn span(&mut self, name: &str, start: i64, attrs: serde_json::Value) {
        self.spans.push(Span {
            trace_id: self.trace.clone(),
            span_id: random_id(8),
            parent_id: Some(self.root.clone()),
            run_id: self.run.clone(),
            name: name.to_owned(),
            start_us: start,
            end_us: now_us(),
            attrs,
        });
    }
}

impl Executor {
    /// An executor on `store`; app files live under `data_dir/files/<app>`. Must be called inside a Tokio runtime
    /// (it starts the log writer).
    ///
    /// # Errors
    /// When the Orbit engine cannot be created.
    pub fn new(
        store: Arc<Store>,
        data_dir: &std::path::Path,
        fuel_metering: bool,
    ) -> anyhow::Result<Self> {
        let queues: Arc<dyn MessageQueue> = Arc::clone(&store) as Arc<dyn MessageQueue>;
        Self::with_queues(store, data_dir, fuel_metering, queues)
    }

    /// Like [`new`](Self::new) with another queue backend (the default is the store itself).
    ///
    /// # Errors
    /// When the Orbit engine cannot be created.
    pub fn with_queues(
        store: Arc<Store>,
        data_dir: &std::path::Path,
        fuel_metering: bool,
        queues: Arc<dyn MessageQueue>,
    ) -> anyhow::Result<Self> {
        let queue_signal = Arc::new(tokio::sync::Notify::new());
        let counters = Arc::new(Counters::default());
        let runs = Arc::new(RunIndex::default());
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(log_writer(Arc::clone(&store), rx));
        let sink = Arc::new(CloudSink {
            runs: Arc::clone(&runs),
            tx: tx.clone(),
            lines: Arc::clone(&counters.log_lines),
        });
        let stdin = RunStdin::default();
        let clock = ClockCapability::system();
        let wasi = WasiCapability::new(SharedSink(Arc::clone(&sink))).with_clock(&clock);
        let registry = CapabilityRegistry::new()
            .with(LogCapability::with_shared_sink(sink))
            .with(clock)
            .with(RandomCapability::new())
            .with(NetCapability::new())
            .with(KvCapability::new(StoreKv(Arc::clone(&store))))
            .with(QueueCapability::new(GuestQueues::new(
                Arc::clone(&queues),
                Arc::clone(&queue_signal),
            )))
            .with(wasi)
            .with(FsCapability::new())
            .with(ProcessCapability::new().with_stdin(stdin.clone()));
        let mut config = RuntimeConfig::default();
        config.engine.fuel_metering = fuel_metering;
        let runtime = Runtime::new(config, registry)?;
        let files_root = data_dir.join("files");
        Ok(Self {
            store,
            runtime,
            runs,
            stdin,
            files_root,
            queues,
            postgres: None,
            queue_signal,
            log_tx: tx,
            counters,
        })
    }

    /// Enables PostgreSQL provisioning (`--postgres-url`).
    #[must_use]
    pub fn with_postgres(mut self, postgres: Arc<Postgres>) -> Self {
        self.postgres = Some(postgres);
        self
    }

    /// The operator's PostgreSQL server, when configured.
    #[must_use]
    pub fn postgres(&self) -> Option<&Arc<Postgres>> {
        self.postgres.as_ref()
    }

    /// What to add to a run of `app`: its `DATABASE_URL` and the grant to reach the server.
    fn injection(&self, app: &str, spec: &Spec) -> Result<Injected, String> {
        let mut inj = Injected::default();
        if !spec.postgres {
            return Ok(inj);
        }
        let Some(pg) = &self.postgres else {
            return Err("the spec asks for postgres but this node has no --postgres-url".into());
        };
        let d = self
            .store
            .app_database(app)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the database of `{app}` is not provisioned"))?;
        inj.env
            .insert("DATABASE_URL".to_owned(), pg.database_url(&d, true));
        inj.connect.push(pg.connect_pattern());
        Ok(inj)
    }

    /// The queue backend (consumers, the queue API and `orbit:queue/v1` all go through it).
    #[must_use]
    pub fn queues(&self) -> &Arc<dyn MessageQueue> {
        &self.queues
    }

    /// Woken after every publish so idle consumers poll at once.
    #[must_use]
    pub fn queue_signal(&self) -> &Arc<tokio::sync::Notify> {
        &self.queue_signal
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Records a run and executes it to the end. The returned run is the finished record.
    ///
    /// # Errors
    /// Only store failures; guest failures are a `failed` run.
    pub async fn run(&self, d: &Deployment, req: RunRequest) -> anyhow::Result<Run> {
        let run = self
            .store
            .start_run(&d.app, d.id, &req.trigger, req.message_id)?;
        self.execute(d, run, req).await
    }

    /// Executes a run that [`Store::start_run`] recorded (services record it first so the supervisor knows the id).
    ///
    /// # Errors
    /// Only store failures.
    pub async fn execute(&self, d: &Deployment, run: Run, req: RunRequest) -> anyhow::Result<Run> {
        self.counters.runs_started.fetch_add(1, Ordering::Relaxed);
        let active = Active::enter(&self.counters);
        let start = now_us();
        let root = random_id(8);
        let mut tl = Timeline {
            trace: run.trace_id.clone(),
            run: run.id.clone(),
            root: root.clone(),
            spans: Vec::new(),
        };
        let outcome = self.attempt(d, &run, req, &mut tl).await;
        drop(active);
        let wall = u64::try_from((now_us() - start) / 1000).unwrap_or(0);
        self.counters
            .run_ms_total
            .fetch_add(wall, Ordering::Relaxed);
        tl.spans.push(Span {
            trace_id: run.trace_id.clone(),
            span_id: root,
            parent_id: None,
            run_id: run.id.clone(),
            name: "run".into(),
            start_us: start,
            end_us: now_us(),
            attrs: serde_json::json!({
                "app": d.app, "deployment": d.id, "version": d.version, "module": d.module, "trigger": run.trigger,
                "status": outcome.status, "exit_code": outcome.exit_code, "error": outcome.error,
            }),
        });
        self.store.insert_spans(&tl.spans)?;
        // The run's log lines are in the store before the run reads as finished.
        self.flush_logs().await;
        self.store.finish_run(&run.id, &outcome)?;
        Ok(self.store.run(&run.id)?)
    }

    /// Removes the app's persistent files directory (`files: true` in its spec), so a new app of the same name starts
    /// empty.
    ///
    /// # Errors
    /// When the directory exists and cannot be removed.
    pub fn remove_app_files(&self, app: &str) -> std::io::Result<()> {
        debug_assert!(oc_store::valid_name(app), "app names are validated");
        match std::fs::remove_dir_all(self.files_root.join(app)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Waits until every log line written so far is stored.
    async fn flush_logs(&self) {
        let (tx, rx) = oneshot::channel();
        if self.log_tx.send(LogMsg::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    async fn attempt(
        &self,
        d: &Deployment,
        run: &Run,
        req: RunRequest,
        tl: &mut Timeline,
    ) -> RunOutcome {
        let failed = |e: String| RunOutcome {
            status: "failed".into(),
            error: Some(e),
            ..RunOutcome::default()
        };
        let spec: Spec = match serde_json::from_value(d.spec.clone()) {
            Ok(s) => s,
            Err(e) => return failed(format!("stored spec is invalid: {e}")),
        };
        // load: the runtime caches compiled artifacts by content hash, so this is cheap after the first run
        let t = now_us();
        let bytes = match self.store.module(&d.module) {
            Ok(b) => b,
            Err(e) => return failed(e.to_string()),
        };
        let artifact = match self.runtime.load(&bytes) {
            Ok(a) => a,
            Err(e) => {
                tl.span("load", t, serde_json::json!({ "error": e.to_string() }));
                return failed(format!("load failed: {e}"));
            }
        };
        tl.span(
            "load",
            t,
            serde_json::json!({ "module": d.module, "bytes": bytes.len() }),
        );
        // instantiate
        let t = now_us();
        let files_dir = if spec.files {
            let dir = self.files_root.join(&d.app);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return failed(format!("cannot create the app's files directory: {e}"));
            }
            match std::fs::canonicalize(&dir) {
                Ok(p) => Some(p),
                Err(e) => return failed(format!("cannot open the app's files directory: {e}")),
            }
        } else {
            None
        };
        let stdin_key = req.stdin.map(|bytes| {
            let key = random_id(8);
            self.stdin
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key.clone(), bytes);
            key
        });
        let injected = match self.injection(&d.app, &spec) {
            Ok(i) => i,
            Err(e) => return failed(e),
        };
        let grants = match spec.grants_with(
            &d.app,
            stdin_key.as_deref(),
            files_dir.as_deref(),
            &injected,
        ) {
            Ok(g) => g,
            Err(e) => return failed(e),
        };
        let spawned = self
            .runtime
            .spawn_isolate(&artifact, d.app.as_str(), &grants, spec.limits())
            .await;
        if let Some(k) = &stdin_key {
            // taken by the isolate if it was created; dropped here otherwise
            self.stdin
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(k);
        }
        let mut iso = match spawned {
            Ok(i) => i,
            Err(e) => {
                tl.span(
                    "instantiate",
                    t,
                    serde_json::json!({ "error": e.to_string() }),
                );
                return failed(format!("instantiate failed: {e}"));
            }
        };
        let iid = iso.id().0;
        self.runs.map().insert(iid, run.id.clone());
        tl.span(
            "instantiate",
            t,
            serde_json::json!({ "instance": iso.id().to_string() }),
        );
        // execute
        let t = now_us();
        let has = |name: &str| {
            artifact
                .info()
                .exports
                .iter()
                .any(|e| e.name == name && e.kind == "func")
        };
        let result = drive(
            &mut iso,
            spec.entry.as_deref(),
            &req.args,
            has("_start"),
            has(ORIN_MAIN),
        )
        .await;
        let m = iso.metrics();
        iso.terminate();
        self.runs.map().remove(&iid);
        let calls: serde_json::Map<String, serde_json::Value> = m
            .host_calls
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        let mut o = match result {
            Ok(code) => RunOutcome {
                status: if code == 0 { "succeeded" } else { "failed" }.into(),
                exit_code: Some(i64::from(code)),
                error: (code != 0).then(|| format!("exited with status {code}")),
                ..RunOutcome::default()
            },
            Err(e) => failed(e),
        };
        o.wall_ms = m
            .last_invocation
            .as_ref()
            .map(|i| i64::try_from(i.wall_time.as_millis()).unwrap_or(i64::MAX));
        o.fuel = m.fuel_consumed.and_then(|f| i64::try_from(f).ok());
        o.peak_memory = i64::try_from(m.peak_memory_bytes).ok();
        o.host_calls = Some(serde_json::Value::Object(calls));
        tl.span(
            "execute",
            t,
            serde_json::json!({
                "entry": spec.entry, "exit_code": o.exit_code, "error": o.error, "fuel": o.fuel,
                "peak_memory_bytes": o.peak_memory, "host_calls": o.host_calls,
            }),
        );
        o
    }
}

/// Counts a run as active while it lives, also when its future is dropped (a stopped service).
struct Active(Arc<Counters>);

impl Active {
    fn enter(c: &Arc<Counters>) -> Self {
        c.runs_active.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(c))
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.0.runs_active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Forwards to the shared sink (the WASI shim takes the sink by value).
struct SharedSink(Arc<CloudSink>);

impl LogSink for SharedSink {
    fn write(&self, r: &LogRecord) {
        self.0.write(r);
    }
}

/// Initialises the isolate and calls its entry point, exactly as `orbit run` does: an explicit `entry`, else
/// `_start` of a command module, else `orin_main` of an Orin program. Returns the exit status.
async fn drive(
    iso: &mut Isolate,
    entry: Option<&str>,
    args: &[i64],
    has_start: bool,
    has_orin_main: bool,
) -> Result<i32, String> {
    match iso.init().await {
        Ok(()) => {}
        Err(ExecError::Exit(code)) => return Ok(code),
        Err(e) => return Err(format!("initialisation failed: {e}")),
    }
    if let Some(code) = iso.exit_code() {
        return Ok(code);
    }
    let name = match entry {
        Some(e) => e,
        None if has_start => "_start",
        None if has_orin_main => ORIN_MAIN,
        None => {
            return Err(
                "the module exports neither `_start` nor `orin_main`; set `entry` in the spec"
                    .into(),
            );
        }
    };
    match iso.invoke(name, args).await {
        Ok(results) if name == ORIN_MAIN => Ok(results
            .first()
            .map_or(0, |r| i32::try_from(*r).unwrap_or(1))),
        Ok(_) => Ok(iso.exit_code().unwrap_or(0)),
        Err(ExecError::Exit(code)) => Ok(code),
        Err(e @ ExecError::Trap(_)) if name == ORIN_MAIN => {
            match iso.post_mortem_i32(ORIN_EXIT_STATUS).await {
                Some(status) if status != -1 => Ok(status),
                _ => Err(format!("`{name}` failed: {e}")),
            }
        }
        Err(e) => Err(format!("`{name}` failed: {e}")),
    }
}
