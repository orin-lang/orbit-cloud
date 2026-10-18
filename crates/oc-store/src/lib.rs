//! Persistence for Orbit Cloud: one SQLite database (WAL mode) holds apps, deployments, modules, runs, log lines,
//! trace spans, the KV data behind `orbit:kv/v1` and the queues. The API is synchronous and every call is short; the
//! connection sits behind a mutex, so callers on an async runtime may call it directly or from `spawn_blocking`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: &str = "1";

/// A store error. SQLite errors are wrapped; `NotFound` and `Conflict` are the expected outcomes callers branch on.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The named object does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// The object exists already, or the operation conflicts with its state.
    #[error("{0}")]
    Conflict(String),
    /// The database failed.
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    /// A stored JSON document is malformed.
    #[error("stored json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result of a store call.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A random identifier of `bytes` random bytes, hex encoded.
#[must_use]
pub fn random_id(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::fill(&mut b[..]);
    hex(&b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Whether `name` is a valid app or queue name: 1 to 63 of `a-z`, `0-9` and `-`, not starting with `-`.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// An app and its active deployment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct App {
    /// The app name (also its Orbit tenant).
    pub name: String,
    /// Creation time.
    pub created_at: i64,
    /// The active deployment, if any.
    pub active_deployment: Option<i64>,
}

/// One immutable version of an app.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Deployment {
    /// Global id.
    pub id: i64,
    /// The app.
    pub app: String,
    /// 1, 2, ... per app.
    pub version: i64,
    /// SHA-256 of the module, hex.
    pub module: String,
    /// The deployment spec (JSON, validated by orbitd).
    pub spec: serde_json::Value,
    /// Creation time.
    pub created_at: i64,
}

/// One execution of a deployment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Run {
    /// Run id (hex).
    pub id: String,
    /// The app.
    pub app: String,
    /// The deployment that ran.
    pub deployment: i64,
    /// What started it: `api`, `service`, `queue`.
    pub trigger: String,
    /// `running`, `succeeded`, `failed`.
    pub status: String,
    /// Exit code of the guest, when it set one.
    pub exit_code: Option<i64>,
    /// Failure text (trap, limit, load error).
    pub error: Option<String>,
    /// Start time.
    pub started_at: i64,
    /// End time.
    pub finished_at: Option<i64>,
    /// Wall time of the execution in milliseconds.
    pub wall_ms: Option<i64>,
    /// Wall time of the execution in microseconds (runs recorded before this field existed have none).
    #[serde(default)]
    pub wall_us: Option<i64>,
    /// Fuel consumed, when metering is on.
    pub fuel: Option<i64>,
    /// Peak linear memory in bytes.
    pub peak_memory: Option<i64>,
    /// Host calls per capability.
    pub host_calls: Option<serde_json::Value>,
    /// The trace of the run.
    pub trace_id: String,
    /// The queue message that triggered it.
    pub message_id: Option<i64>,
}

/// One time bucket of [`Stats`] (also used for the totals, whose `t` is the start of the range).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatsBucket {
    /// Start of the bucket (Unix ms).
    pub t: i64,
    /// Runs that succeeded and started in the bucket.
    pub succeeded: i64,
    /// Runs that failed.
    pub failed: i64,
    /// Runs still running.
    pub running: i64,
    /// Median run time in microseconds.
    pub p50_us: Option<i64>,
    /// 95th percentile of the run time in microseconds.
    pub p95_us: Option<i64>,
    /// Log lines written in the bucket.
    pub logs: i64,
}

/// Run statistics over a time range, see [`Store::stats`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stats {
    /// Start of the range (Unix ms, inclusive).
    pub since: i64,
    /// End of the range (exclusive).
    pub until: i64,
    /// Bucket width in milliseconds.
    pub bucket_ms: i64,
    /// One entry per bucket, oldest first, empty buckets included.
    pub buckets: Vec<StatsBucket>,
    /// The whole range in one bucket.
    pub totals: StatsBucket,
}

/// The outcome recorded when a run ends.
#[derive(Clone, Debug, Default)]
pub struct RunOutcome {
    /// `succeeded` or `failed`.
    pub status: String,
    /// Exit code, when the guest set one.
    pub exit_code: Option<i64>,
    /// Failure text.
    pub error: Option<String>,
    /// Wall time in milliseconds.
    pub wall_ms: Option<i64>,
    /// Wall time in microseconds.
    pub wall_us: Option<i64>,
    /// Fuel consumed.
    pub fuel: Option<i64>,
    /// Peak linear memory in bytes.
    pub peak_memory: Option<i64>,
    /// Host calls per capability.
    pub host_calls: Option<serde_json::Value>,
}

/// One log line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogLine {
    /// Monotonic id (the cursor for tailing).
    pub id: i64,
    /// Time it was accepted.
    pub ts: i64,
    /// The app.
    pub app: String,
    /// The run, when known.
    pub run_id: Option<String>,
    /// `trace`, `debug`, `info`, `warn`, `error`.
    pub level: String,
    /// The text.
    pub message: String,
}

/// A log line to append (the id and time are assigned by the caller's batch).
#[derive(Clone, Debug)]
pub struct NewLog {
    /// Time it was accepted.
    pub ts: i64,
    /// The app.
    pub app: String,
    /// The run, when known.
    pub run_id: Option<String>,
    /// Level name.
    pub level: String,
    /// The text.
    pub message: String,
}

/// A log query. All fields narrow the result; `after` is an exclusive id cursor.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LogQuery {
    /// Only this app.
    pub app: Option<String>,
    /// Only this run.
    pub run: Option<String>,
    /// Only this level.
    pub level: Option<String>,
    /// Lines with an id greater than this.
    pub after: Option<i64>,
    /// Lines at or after this time.
    pub since: Option<i64>,
    /// Lines whose text contains this.
    pub contains: Option<String>,
    /// At most this many (default 200, at most 5000).
    pub limit: Option<i64>,
}

/// One span of a trace.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Span {
    /// Trace id (32 hex digits, W3C size).
    pub trace_id: String,
    /// Span id (16 hex digits).
    pub span_id: String,
    /// Parent span id.
    pub parent_id: Option<String>,
    /// The run.
    pub run_id: String,
    /// Span name.
    pub name: String,
    /// Start, microseconds since the Unix epoch.
    pub start_us: i64,
    /// End, microseconds since the Unix epoch.
    pub end_us: i64,
    /// Attributes.
    pub attrs: serde_json::Value,
}

/// A KV entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvEntry {
    /// The key.
    pub key: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
    /// Last write.
    pub updated_at: i64,
}

/// A queue message handed to a consumer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    /// Message id.
    pub id: i64,
    /// The queue.
    pub queue: String,
    /// The payload.
    pub body: Vec<u8>,
    /// Deliveries so far, including this one.
    pub attempts: i64,
    /// Enqueue time.
    pub enqueued_at: i64,
    /// The token that acks or nacks this delivery.
    pub lease_token: String,
    /// The error of the previous attempt.
    pub last_error: Option<String>,
}

/// Counts of one queue.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueStats {
    /// The queue.
    pub queue: String,
    /// Ready (including expired leases).
    pub ready: i64,
    /// Leased and not expired.
    pub leased: i64,
    /// Dead letters.
    pub dead: i64,
    /// Acknowledged.
    pub done: i64,
}

/// What a nack did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NackOutcome {
    /// Will be delivered again after the delay.
    Retry,
    /// `max_attempts` reached: moved to the dead letters.
    Dead,
}

/// The database.
pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// A span as read from SQLite, before its attributes are parsed.
type SpanRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
    String,
);

fn run_from_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let calls: Option<String> = r.get("host_calls")?;
    Ok(Run {
        id: r.get("id")?,
        app: r.get("app")?,
        deployment: r.get("deployment")?,
        trigger: r.get("trigger")?,
        status: r.get("status")?,
        exit_code: r.get("exit_code")?,
        error: r.get("error")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
        wall_ms: r.get("wall_ms")?,
        wall_us: r.get("wall_us")?,
        fuel: r.get("fuel")?,
        peak_memory: r.get("peak_memory")?,
        host_calls: calls.and_then(|c| serde_json::from_str(&c).ok()),
        trace_id: r.get("trace_id")?,
        message_id: r.get("message_id")?,
    })
}

fn deployment_from_row(r: &Row<'_>) -> rusqlite::Result<(i64, String, i64, String, String, i64)> {
    Ok((
        r.get("id")?,
        r.get("app")?,
        r.get("version")?,
        r.get("module")?,
        r.get("spec")?,
        r.get("created_at")?,
    ))
}

fn to_deployment(t: (i64, String, i64, String, String, i64)) -> Result<Deployment> {
    Ok(Deployment {
        id: t.0,
        app: t.1,
        version: t.2,
        module: t.3,
        spec: serde_json::from_str(&t.4)?,
        created_at: t.5,
    })
}

fn log_from_row(r: &Row<'_>) -> rusqlite::Result<LogLine> {
    Ok(LogLine {
        id: r.get("id")?,
        ts: r.get("ts")?,
        app: r.get("app")?,
        run_id: r.get("run_id")?,
        level: r.get("level")?,
        message: r.get("message")?,
    })
}

impl Store {
    /// Opens (and creates or migrates) the database at `path`.
    ///
    /// # Errors
    /// [`StoreError::Db`] if SQLite cannot open or migrate it.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// An in-memory database (tests).
    ///
    /// # Errors
    /// [`StoreError::Db`] if SQLite fails.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // Databases created before `wall_us` existed get the column (NULL for their old runs).
        let has_wall_us = conn
            .prepare("SELECT 1 FROM pragma_table_info('runs') WHERE name = 'wall_us'")?
            .exists([])?;
        if !has_wall_us {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN wall_us INTEGER")?;
        }
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite consistent (every statement is atomic), so a poisoned lock is
        // still usable.
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks runs left `running` by a previous process as failed. Returns how many.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn recover(&self) -> Result<usize> {
        let n = self.db().execute(
            "UPDATE runs SET status = 'failed', error = 'orbitd stopped while the run was active', finished_at = ?1
             WHERE status = 'running'",
            params![now_ms()],
        )?;
        Ok(n)
    }

    // ------------------------------------------------------------------------------------------------ apps

    /// Creates an app.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad or taken name.
    pub fn create_app(&self, name: &str) -> Result<App> {
        if !valid_name(name) {
            return Err(StoreError::Conflict(format!(
                "invalid app name `{name}`: 1-63 of a-z, 0-9 and -, not starting with -"
            )));
        }
        let now = now_ms();
        let n = self.db().execute(
            "INSERT OR IGNORE INTO apps (name, created_at) VALUES (?1, ?2)",
            params![name, now],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict(format!("app `{name}` exists")));
        }
        Ok(App {
            name: name.to_owned(),
            created_at: now,
            active_deployment: None,
        })
    }

    /// The app `name`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn app(&self, name: &str) -> Result<App> {
        self.db()
            .query_row(
                "SELECT name, created_at, active_deployment FROM apps WHERE name = ?1",
                params![name],
                |r| {
                    Ok(App {
                        name: r.get(0)?,
                        created_at: r.get(1)?,
                        active_deployment: r.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("app `{name}`")))
    }

    /// All apps by name.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn apps(&self) -> Result<Vec<App>> {
        let db = self.db();
        let mut st =
            db.prepare("SELECT name, created_at, active_deployment FROM apps ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(App {
                name: r.get(0)?,
                created_at: r.get(1)?,
                active_deployment: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes an app with its deployments, runs, logs, spans and KV data. Modules stay (content-addressed, shared).
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn delete_app(&self, name: &str) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let n = tx.execute("DELETE FROM apps WHERE name = ?1", params![name])?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("app `{name}`")));
        }
        tx.execute(
            "DELETE FROM spans WHERE run_id IN (SELECT id FROM runs WHERE app = ?1)",
            params![name],
        )?;
        tx.execute("DELETE FROM runs WHERE app = ?1", params![name])?;
        tx.execute("DELETE FROM logs WHERE app = ?1", params![name])?;
        tx.execute("DELETE FROM kv WHERE tenant = ?1", params![name])?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ modules

    /// Stores a module (idempotent) and returns its SHA-256 (hex).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn put_module(&self, bytes: &[u8]) -> Result<String> {
        let sha = hex(&Sha256::digest(bytes));
        self.db().execute(
            "INSERT OR IGNORE INTO modules (sha256, bytes, size, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![sha, bytes, i64::try_from(bytes.len()).unwrap_or(i64::MAX), now_ms()],
        )?;
        Ok(sha)
    }

    /// The bytes of a module.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn module(&self, sha: &str) -> Result<Vec<u8>> {
        self.db()
            .query_row(
                "SELECT bytes FROM modules WHERE sha256 = ?1",
                params![sha],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("module {sha}")))
    }

    // ------------------------------------------------------------------------------------------------ deployments

    /// Creates the next version of `app` (creating the app if needed) and makes it active.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the module is unknown, [`StoreError::Conflict`] for a bad app name.
    pub fn deploy(&self, app: &str, module: &str, spec: &serde_json::Value) -> Result<Deployment> {
        if !valid_name(app) {
            return Err(StoreError::Conflict(format!("invalid app name `{app}`")));
        }
        let now = now_ms();
        let mut db = self.db();
        let tx = db.transaction()?;
        let known: bool = tx
            .query_row(
                "SELECT 1 FROM modules WHERE sha256 = ?1",
                params![module],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !known {
            return Err(StoreError::NotFound(format!("module {module}")));
        }
        tx.execute(
            "INSERT OR IGNORE INTO apps (name, created_at) VALUES (?1, ?2)",
            params![app, now],
        )?;
        let version: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM deployments WHERE app = ?1",
            params![app],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO deployments (app, version, module, spec, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![app, version, module, spec.to_string(), now],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE apps SET active_deployment = ?1 WHERE name = ?2",
            params![id, app],
        )?;
        tx.commit()?;
        Ok(Deployment {
            id,
            app: app.to_owned(),
            version,
            module: module.to_owned(),
            spec: spec.clone(),
            created_at: now,
        })
    }

    /// A deployment by id.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn deployment(&self, id: i64) -> Result<Deployment> {
        let t = self
            .db()
            .query_row(
                "SELECT id, app, version, module, spec, created_at FROM deployments WHERE id = ?1",
                params![id],
                deployment_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("deployment {id}")))?;
        to_deployment(t)
    }

    /// The deployments of `app`, newest first.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn deployments(&self, app: &str) -> Result<Vec<Deployment>> {
        let rows: Vec<_> = {
            let db = self.db();
            let mut st = db.prepare(
                "SELECT id, app, version, module, spec, created_at FROM deployments WHERE app = ?1 ORDER BY version DESC",
            )?;
            let it = st.query_map(params![app], deployment_from_row)?;
            it.collect::<rusqlite::Result<_>>()?
        };
        rows.into_iter().map(to_deployment).collect()
    }

    /// Makes deployment `id` of `app` active (rollback).
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the deployment is not one of `app`.
    pub fn activate(&self, app: &str, id: i64) -> Result<()> {
        let n = self.db().execute(
            "UPDATE apps SET active_deployment = ?1 WHERE name = ?2 AND EXISTS
               (SELECT 1 FROM deployments WHERE id = ?1 AND app = ?2)",
            params![id, app],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("deployment {id} of `{app}`")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ runs

    /// Records the start of a run.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn start_run(
        &self,
        app: &str,
        deployment: i64,
        trigger: &str,
        message_id: Option<i64>,
    ) -> Result<Run> {
        let run = Run {
            id: random_id(8),
            app: app.to_owned(),
            deployment,
            trigger: trigger.to_owned(),
            status: "running".to_owned(),
            exit_code: None,
            error: None,
            started_at: now_ms(),
            finished_at: None,
            wall_ms: None,
            wall_us: None,
            fuel: None,
            peak_memory: None,
            host_calls: None,
            trace_id: random_id(16),
            message_id,
        };
        self.db().execute(
            "INSERT INTO runs (id, app, deployment, trigger, status, started_at, trace_id, message_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![run.id, run.app, run.deployment, run.trigger, run.status, run.started_at, run.trace_id, message_id],
        )?;
        Ok(run)
    }

    /// Records the end of a run.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn finish_run(&self, id: &str, o: &RunOutcome) -> Result<()> {
        let n = self.db().execute(
            "UPDATE runs SET status = ?2, exit_code = ?3, error = ?4, finished_at = ?5, wall_ms = ?6, fuel = ?7,
               peak_memory = ?8, host_calls = ?9, wall_us = ?10 WHERE id = ?1",
            params![
                id,
                o.status,
                o.exit_code,
                o.error,
                now_ms(),
                o.wall_ms,
                o.fuel,
                o.peak_memory,
                o.host_calls.as_ref().map(serde_json::Value::to_string),
                o.wall_us
            ],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("run {id}")));
        }
        Ok(())
    }

    /// A run by id.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn run(&self, id: &str) -> Result<Run> {
        self.db()
            .query_row(
                "SELECT * FROM runs WHERE id = ?1",
                params![id],
                run_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("run {id}")))
    }

    /// Runs, newest first; of one app when `app` is given; at most `limit` (1..=1000).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn runs(&self, app: Option<&str>, limit: i64) -> Result<Vec<Run>> {
        let limit = limit.clamp(1, 1000);
        let db = self.db();
        let mut st = db.prepare(
            "SELECT * FROM runs WHERE (?1 IS NULL OR app = ?1) ORDER BY started_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![app, limit], run_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Run counts, run-time percentiles and log counts per time bucket, for charts. `since` (inclusive) and `until`
    /// (exclusive) are Unix milliseconds, `bucket_ms` the bucket width; every bucket in the range is returned, empty ones
    /// with zero counts. Run time is `wall_us`, or `wall_ms` of runs that predate it, in microseconds. Percentiles are
    /// nearest-rank and `None` for a bucket without finished runs.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for an empty range, a bucket under 1 s, or more than 1000 buckets; [`StoreError::Db`].
    pub fn stats(
        &self,
        since: i64,
        until: i64,
        bucket_ms: i64,
        app: Option<&str>,
    ) -> Result<Stats> {
        if until <= since || bucket_ms < 1000 || (until - since + bucket_ms - 1) / bucket_ms > 1000
        {
            return Err(StoreError::Conflict(
                "stats needs since < until, a bucket of at least 1000 ms and at most 1000 buckets"
                    .into(),
            ));
        }
        let n = usize::try_from((until - since + bucket_ms - 1) / bucket_ms).unwrap_or(0);
        let mut buckets: Vec<StatsBucket> = (0..n)
            .map(|i| StatsBucket {
                t: since + i64::try_from(i).unwrap_or(0) * bucket_ms,
                ..StatsBucket::default()
            })
            .collect();
        let db = self.db();
        let index = |b: i64| usize::try_from(b).ok().filter(|i| *i < n);
        {
            let mut st = db.prepare(
                "SELECT (started_at - ?1) / ?3 AS b, status, COUNT(*) FROM runs
                 WHERE started_at >= ?1 AND started_at < ?2 AND (?4 IS NULL OR app = ?4) GROUP BY b, status",
            )?;
            let rows = st.query_map(params![since, until, bucket_ms, app], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (b, status, count) = row?;
                if let Some(i) = index(b) {
                    match status.as_str() {
                        "succeeded" => buckets[i].succeeded = count,
                        "failed" => buckets[i].failed = count,
                        _ => buckets[i].running += count,
                    }
                }
            }
        }
        // Nearest-rank percentiles with window functions: the smallest duration whose rank is at least p% of the count.
        let percentiles = |group: &str| {
            format!(
                "WITH r AS (SELECT {group} AS b, COALESCE(wall_us, wall_ms * 1000) AS d FROM runs
                            WHERE started_at >= ?1 AND started_at < ?2 AND (?4 IS NULL OR app = ?4)),
                      k AS (SELECT b, d, ROW_NUMBER() OVER (PARTITION BY b ORDER BY d) AS rn,
                                   COUNT(*) OVER (PARTITION BY b) AS n FROM r WHERE d IS NOT NULL)
                 SELECT b, MIN(CASE WHEN rn * 100 >= 50 * n THEN d END), MIN(CASE WHEN rn * 100 >= 95 * n THEN d END)
                 FROM k GROUP BY b"
            )
        };
        {
            let mut st = db.prepare(&percentiles("(started_at - ?1) / ?3"))?;
            let rows = st.query_map(params![since, until, bucket_ms, app], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })?;
            for row in rows {
                let (b, p50, p95) = row?;
                if let Some(i) = index(b) {
                    buckets[i].p50_us = p50;
                    buckets[i].p95_us = p95;
                }
            }
        }
        let mut totals = StatsBucket {
            t: since,
            ..StatsBucket::default()
        };
        {
            let mut st = db.prepare(&percentiles("0"))?;
            let row = st
                .query_row(params![since, until, bucket_ms, app], |r| {
                    Ok((r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?))
                })
                .optional()?;
            if let Some((p50, p95)) = row {
                totals.p50_us = p50;
                totals.p95_us = p95;
            }
        }
        {
            let mut st = db.prepare(
                "SELECT (ts - ?1) / ?3 AS b, COUNT(*) FROM logs
                 WHERE ts >= ?1 AND ts < ?2 AND (?4 IS NULL OR app = ?4) GROUP BY b",
            )?;
            let rows = st.query_map(params![since, until, bucket_ms, app], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (b, count) = row?;
                if let Some(i) = index(b) {
                    buckets[i].logs = count;
                }
            }
        }
        for b in &buckets {
            totals.succeeded += b.succeeded;
            totals.failed += b.failed;
            totals.running += b.running;
            totals.logs += b.logs;
        }
        Ok(Stats {
            since,
            until,
            bucket_ms,
            buckets,
            totals,
        })
    }

    /// Counts of runs by app and status (for metrics).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn run_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT app, status, COUNT(*) FROM runs GROUP BY app, status ORDER BY app, status",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------------------------------------------ logs

    /// Appends log lines in one transaction.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn append_logs(&self, lines: &[NewLog]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO logs (ts, app, run_id, level, message) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for l in lines {
                st.execute(params![l.ts, l.app, l.run_id, l.level, l.message])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Log lines matching `q`, oldest first.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn logs(&self, q: &LogQuery) -> Result<Vec<LogLine>> {
        let limit = q.limit.unwrap_or(200).clamp(1, 5000);
        let db = self.db();
        // Newest `limit` lines that match, returned in ascending order.
        let mut st = db.prepare(
            "SELECT * FROM (SELECT * FROM logs WHERE (?1 IS NULL OR app = ?1) AND (?2 IS NULL OR run_id = ?2)
               AND (?3 IS NULL OR level = ?3) AND (?4 IS NULL OR id > ?4) AND (?5 IS NULL OR ts >= ?5)
               AND (?6 IS NULL OR instr(message, ?6) > 0)
             ORDER BY id DESC LIMIT ?7) ORDER BY id ASC",
        )?;
        let rows = st.query_map(
            params![q.app, q.run, q.level, q.after, q.since, q.contains, limit],
            log_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The number of stored log lines (metrics).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn log_count(&self) -> Result<i64> {
        Ok(self
            .db()
            .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------------------------------------------ spans

    /// Stores spans.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn insert_spans(&self, spans: &[Span]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO spans (trace_id, span_id, parent_id, run_id, name, start_us, end_us, attrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for s in spans {
                st.execute(params![
                    s.trace_id,
                    s.span_id,
                    s.parent_id,
                    s.run_id,
                    s.name,
                    s.start_us,
                    s.end_us,
                    s.attrs.to_string()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The spans of a trace, by start time.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn trace(&self, trace_id: &str) -> Result<Vec<Span>> {
        let rows: Vec<SpanRow> = {
            let db = self.db();
            let mut st = db.prepare(
                "SELECT trace_id, span_id, parent_id, run_id, name, start_us, end_us, attrs FROM spans
                 WHERE trace_id = ?1 ORDER BY start_us, span_id",
            )?;
            let it = st.query_map(params![trace_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };
        rows.into_iter()
            .map(|t| {
                Ok(Span {
                    trace_id: t.0,
                    span_id: t.1,
                    parent_id: t.2,
                    run_id: t.3,
                    name: t.4,
                    start_us: t.5,
                    end_us: t.6,
                    attrs: serde_json::from_str(&t.7)?,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------------------------------------------ kv

    /// The value of a key.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn kv_get(&self, tenant: &str, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db()
            .query_row(
                "SELECT value FROM kv WHERE tenant = ?1 AND ns = ?2 AND key = ?3",
                params![tenant, ns, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stores a value.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn kv_put(&self, tenant: &str, ns: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.db().execute(
            "INSERT INTO kv (tenant, ns, key, value, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, ns, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![tenant, ns, key, value, now_ms()],
        )?;
        Ok(())
    }

    /// Removes a key; true when it existed.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn kv_delete(&self, tenant: &str, ns: &str, key: &[u8]) -> Result<bool> {
        Ok(self.db().execute(
            "DELETE FROM kv WHERE tenant = ?1 AND ns = ?2 AND key = ?3",
            params![tenant, ns, key],
        )? > 0)
    }

    /// Entries of a namespace whose key starts with `prefix`, by key, at most `limit` (1..=1000).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn kv_list(
        &self,
        tenant: &str,
        ns: &str,
        prefix: &[u8],
        limit: i64,
    ) -> Result<Vec<KvEntry>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT key, value, updated_at FROM kv WHERE tenant = ?1 AND ns = ?2 AND substr(key, 1, ?3) = ?4
             ORDER BY key LIMIT ?5",
        )?;
        let rows = st.query_map(
            params![
                tenant,
                ns,
                i64::try_from(prefix.len()).unwrap_or(i64::MAX),
                prefix,
                limit.clamp(1, 1000)
            ],
            |r| {
                Ok(KvEntry {
                    key: r.get(0)?,
                    value: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The namespaces of a tenant with their entry counts.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn kv_namespaces(&self, tenant: &str) -> Result<Vec<(String, i64)>> {
        let db = self.db();
        let mut st =
            db.prepare("SELECT ns, COUNT(*) FROM kv WHERE tenant = ?1 GROUP BY ns ORDER BY ns")?;
        let rows = st.query_map(params![tenant], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------------------------------------------ queues

    /// Enqueues a message; returns its id.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad queue name.
    pub fn enqueue(&self, queue: &str, body: &[u8]) -> Result<i64> {
        if !valid_name(queue) {
            return Err(StoreError::Conflict(format!(
                "invalid queue name `{queue}`"
            )));
        }
        let now = now_ms();
        let db = self.db();
        db.execute(
            "INSERT INTO queue_messages (queue, body, state, visible_at, enqueued_at) VALUES (?1, ?2, 'ready', ?3, ?3)",
            params![queue, body, now],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Leases the oldest deliverable message of `queue` for `lease_ms`, or `None` when there is none.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn lease(&self, queue: &str, lease_ms: i64) -> Result<Option<Message>> {
        let now = now_ms();
        let token = random_id(12);
        let mut db = self.db();
        let tx = db.transaction()?;
        let found = tx
            .query_row(
                "SELECT id, body, attempts, enqueued_at, last_error FROM queue_messages
                 WHERE queue = ?1 AND state IN ('ready', 'leased') AND visible_at <= ?2 ORDER BY id LIMIT 1",
                params![queue, now],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, body, attempts, enqueued_at, last_error)) = found else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE queue_messages SET state = 'leased', attempts = attempts + 1, visible_at = ?2, lease_token = ?3
             WHERE id = ?1",
            params![id, now.saturating_add(lease_ms.max(1)), token],
        )?;
        tx.commit()?;
        Ok(Some(Message {
            id,
            queue: queue.to_owned(),
            body,
            attempts: attempts + 1,
            enqueued_at,
            lease_token: token,
            last_error,
        }))
    }

    /// Acknowledges a delivery: the message is done.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the lease is not the current one (expired and re-delivered).
    pub fn ack(&self, id: i64, token: &str) -> Result<()> {
        let n = self.db().execute(
            "UPDATE queue_messages SET state = 'done', lease_token = NULL WHERE id = ?1 AND lease_token = ?2
               AND state = 'leased'",
            params![id, token],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict(format!(
                "message {id}: the lease is not current"
            )));
        }
        Ok(())
    }

    /// Rejects a delivery: deliver again after `delay_ms`, or dead-letter when `max_attempts` is reached.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the lease is not the current one.
    pub fn nack(
        &self,
        id: i64,
        token: &str,
        error: &str,
        delay_ms: i64,
        max_attempts: i64,
    ) -> Result<NackOutcome> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let attempts: Option<i64> = tx
            .query_row(
                "SELECT attempts FROM queue_messages WHERE id = ?1 AND lease_token = ?2 AND state = 'leased'",
                params![id, token],
                |r| r.get(0),
            )
            .optional()?;
        let Some(attempts) = attempts else {
            return Err(StoreError::Conflict(format!(
                "message {id}: the lease is not current"
            )));
        };
        let outcome = if attempts >= max_attempts {
            NackOutcome::Dead
        } else {
            NackOutcome::Retry
        };
        let state = if outcome == NackOutcome::Dead {
            "dead"
        } else {
            "ready"
        };
        tx.execute(
            "UPDATE queue_messages SET state = ?2, visible_at = ?3, lease_token = NULL, last_error = ?4 WHERE id = ?1",
            params![id, state, now_ms().saturating_add(delay_ms.max(0)), error],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Counts per queue.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn queues(&self) -> Result<Vec<QueueStats>> {
        let now = now_ms();
        let db = self.db();
        let mut st = db.prepare(
            "SELECT queue,
               SUM(CASE WHEN state = 'ready' OR (state = 'leased' AND visible_at <= ?1) THEN 1 ELSE 0 END),
               SUM(CASE WHEN state = 'leased' AND visible_at > ?1 THEN 1 ELSE 0 END),
               SUM(CASE WHEN state = 'dead' THEN 1 ELSE 0 END),
               SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END)
             FROM queue_messages GROUP BY queue ORDER BY queue",
        )?;
        let rows = st.query_map(params![now], |r| {
            Ok(QueueStats {
                queue: r.get(0)?,
                ready: r.get(1)?,
                leased: r.get(2)?,
                dead: r.get(3)?,
                done: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Dead letters of a queue, oldest first (lease token empty).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn dead_letters(&self, queue: &str, limit: i64) -> Result<Vec<Message>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT id, body, attempts, enqueued_at, last_error FROM queue_messages WHERE queue = ?1 AND state = 'dead'
             ORDER BY id LIMIT ?2",
        )?;
        let rows = st.query_map(params![queue, limit.clamp(1, 1000)], |r| {
            Ok(Message {
                id: r.get(0)?,
                queue: queue.to_owned(),
                body: r.get(1)?,
                attempts: r.get(2)?,
                enqueued_at: r.get(3)?,
                lease_token: String::new(),
                last_error: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Makes every dead letter of `queue` ready again with its attempts reset; returns how many.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn redrive(&self, queue: &str) -> Result<usize> {
        Ok(self.db().execute(
            "UPDATE queue_messages SET state = 'ready', attempts = 0, visible_at = ?2 WHERE queue = ?1 AND state = 'dead'",
            params![queue, now_ms()],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn names() {
        assert!(valid_name("hello-world2"));
        assert!(!valid_name(""));
        assert!(!valid_name("-x"));
        assert!(!valid_name("Hello"));
        assert!(!valid_name(&"a".repeat(64)));
    }

    #[test]
    fn deploy_versions_and_rollback() {
        let s = store();
        assert!(matches!(
            s.deploy("a", "nope", &serde_json::json!({})),
            Err(StoreError::NotFound(_))
        ));
        let m = s.put_module(b"\0asm").unwrap();
        assert_eq!(m, s.put_module(b"\0asm").unwrap(), "content-addressed");
        let d1 = s
            .deploy("a", &m, &serde_json::json!({"kind": "job"}))
            .unwrap();
        let d2 = s
            .deploy("a", &m, &serde_json::json!({"kind": "job", "args": ["x"]}))
            .unwrap();
        assert_eq!((d1.version, d2.version), (1, 2));
        assert_eq!(s.app("a").unwrap().active_deployment, Some(d2.id));
        s.activate("a", d1.id).unwrap();
        assert_eq!(s.app("a").unwrap().active_deployment, Some(d1.id));
        assert!(s.activate("b", d1.id).is_err());
        assert_eq!(s.deployments("a").unwrap().len(), 2);
        assert_eq!(s.module(&m).unwrap(), b"\0asm");
        assert!(matches!(s.create_app("a"), Err(StoreError::Conflict(_))));
    }

    #[test]
    fn runs_logs_and_spans() {
        let s = store();
        let m = s.put_module(b"m").unwrap();
        let d = s.deploy("a", &m, &serde_json::json!({})).unwrap();
        let r = s.start_run("a", d.id, "api", None).unwrap();
        assert_eq!(r.trace_id.len(), 32);
        s.append_logs(&[
            NewLog {
                ts: 1,
                app: "a".into(),
                run_id: Some(r.id.clone()),
                level: "info".into(),
                message: "one".into(),
            },
            NewLog {
                ts: 2,
                app: "a".into(),
                run_id: Some(r.id.clone()),
                level: "error".into(),
                message: "two".into(),
            },
        ])
        .unwrap();
        let all = s
            .logs(&LogQuery {
                app: Some("a".into()),
                ..LogQuery::default()
            })
            .unwrap();
        assert_eq!(
            all.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        let after = s
            .logs(&LogQuery {
                after: Some(all[0].id),
                ..LogQuery::default()
            })
            .unwrap();
        assert_eq!(after.len(), 1);
        let errs = s
            .logs(&LogQuery {
                level: Some("error".into()),
                ..LogQuery::default()
            })
            .unwrap();
        assert_eq!(errs[0].message, "two");
        let tail = s
            .logs(&LogQuery {
                limit: Some(1),
                ..LogQuery::default()
            })
            .unwrap();
        assert_eq!(tail[0].message, "two", "a limit keeps the newest lines");
        s.finish_run(
            &r.id,
            &RunOutcome {
                status: "succeeded".into(),
                exit_code: Some(0),
                ..RunOutcome::default()
            },
        )
        .unwrap();
        assert_eq!(s.run(&r.id).unwrap().status, "succeeded");
        assert_eq!(
            s.run_counts().unwrap(),
            vec![("a".into(), "succeeded".into(), 1)]
        );
        let span = Span {
            trace_id: r.trace_id.clone(),
            span_id: random_id(8),
            parent_id: None,
            run_id: r.id.clone(),
            name: "run".into(),
            start_us: 1,
            end_us: 5,
            attrs: serde_json::json!({"k": 1}),
        };
        s.insert_spans(std::slice::from_ref(&span)).unwrap();
        assert_eq!(s.trace(&r.trace_id).unwrap(), vec![span]);
        let r2 = s.start_run("a", d.id, "api", None).unwrap();
        assert_eq!(s.recover().unwrap(), 1);
        assert_eq!(s.run(&r2.id).unwrap().status, "failed");
        s.delete_app("a").unwrap();
        assert!(s.runs(Some("a"), 10).unwrap().is_empty());
        assert!(s.logs(&LogQuery::default()).unwrap().is_empty());
    }

    #[test]
    fn kv() {
        let s = store();
        s.kv_put("a", "ns", b"k1", b"v1").unwrap();
        s.kv_put("a", "ns", b"k1", b"v2").unwrap();
        s.kv_put("a", "ns", b"x", b"y").unwrap();
        s.kv_put("b", "ns", b"k1", b"other tenant").unwrap();
        assert_eq!(
            s.kv_get("a", "ns", b"k1").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
        assert_eq!(s.kv_list("a", "ns", b"k", 10).unwrap().len(), 1);
        assert_eq!(s.kv_namespaces("a").unwrap(), vec![("ns".into(), 2)]);
        assert!(s.kv_delete("a", "ns", b"k1").unwrap());
        assert!(!s.kv_delete("a", "ns", b"k1").unwrap());
        assert_eq!(
            s.kv_get("b", "ns", b"k1").unwrap().as_deref(),
            Some(&b"other tenant"[..])
        );
    }

    #[test]
    fn queue_lease_ack_nack_dead_redrive() {
        let s = store();
        assert!(s.enqueue("Bad", b"x").is_err());
        let a = s.enqueue("q", b"a").unwrap();
        let b = s.enqueue("q", b"b").unwrap();
        let m = s.lease("q", 60_000).unwrap().unwrap();
        assert_eq!((m.id, m.attempts), (a, 1));
        let m2 = s.lease("q", 60_000).unwrap().unwrap();
        assert_eq!(m2.id, b, "a leased message is not delivered twice");
        assert!(s.lease("q", 60_000).unwrap().is_none());
        s.ack(m.id, &m.lease_token).unwrap();
        assert!(s.ack(m.id, &m.lease_token).is_err());
        assert_eq!(
            s.nack(m2.id, &m2.lease_token, "boom", 0, 2).unwrap(),
            NackOutcome::Retry
        );
        let again = s.lease("q", 60_000).unwrap().unwrap();
        assert_eq!(
            (again.id, again.attempts, again.last_error.as_deref()),
            (b, 2, Some("boom"))
        );
        assert_eq!(
            s.nack(again.id, &again.lease_token, "boom", 0, 2).unwrap(),
            NackOutcome::Dead
        );
        assert!(s.lease("q", 60_000).unwrap().is_none());
        assert_eq!(s.dead_letters("q", 10).unwrap().len(), 1);
        let st = &s.queues().unwrap()[0];
        assert_eq!((st.ready, st.leased, st.dead, st.done), (0, 0, 1, 1));
        assert_eq!(s.redrive("q").unwrap(), 1);
        assert_eq!(s.lease("q", 60_000).unwrap().unwrap().attempts, 1);
    }

    #[test]
    fn expired_lease_is_redelivered() {
        let s = store();
        s.enqueue("q", b"a").unwrap();
        let m = s.lease("q", 1).unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let again = s.lease("q", 60_000).unwrap().unwrap();
        assert_eq!(again.id, m.id);
        assert!(
            s.ack(m.id, &m.lease_token).is_err(),
            "the old lease is void"
        );
        s.ack(again.id, &again.lease_token).unwrap();
    }

    #[test]
    fn file_database_persists() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oc.db");
        {
            let s = Store::open(&p).unwrap();
            s.kv_put("a", "n", b"k", b"v").unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(
            s.kv_get("a", "n", b"k").unwrap().as_deref(),
            Some(&b"v"[..])
        );
    }

    fn old_run(s: &Store, app: &str, at: i64, status: &str, us: Option<i64>, ms: Option<i64>) {
        let r = s.start_run(app, 1, "api", None).unwrap();
        s.db()
            .execute(
                "UPDATE runs SET started_at = ?2, status = ?3, wall_us = ?4, wall_ms = ?5 WHERE id = ?1",
                params![r.id, at, status, us, ms],
            )
            .unwrap();
    }

    #[test]
    fn stats_buckets_counts_and_percentiles() {
        let s = store();
        // bucket 0 (t 0..60_000): ten runs of 1..=10 ms, one failed; bucket 2: one running run; bucket 1 stays empty
        for i in 1..=10 {
            let status = if i == 3 { "failed" } else { "succeeded" };
            old_run(&s, "a", 1000 * i, status, Some(i * 1000), None);
        }
        old_run(&s, "a", 125_000, "running", None, None);
        old_run(&s, "b", 5000, "succeeded", Some(777_000), None);
        s.append_logs(&[NewLog {
            ts: 61_000,
            app: "a".into(),
            run_id: None,
            level: "info".into(),
            message: "x".into(),
        }])
        .unwrap();
        let st = s.stats(0, 180_000, 60_000, Some("a")).unwrap();
        assert_eq!(st.buckets.len(), 3, "every bucket is present");
        let b0 = &st.buckets[0];
        assert_eq!((b0.t, b0.succeeded, b0.failed, b0.running), (0, 9, 1, 0));
        assert_eq!(
            (b0.p50_us, b0.p95_us),
            (Some(5000), Some(10_000)),
            "nearest rank"
        );
        assert_eq!(
            st.buckets[1],
            StatsBucket {
                t: 60_000,
                logs: 1,
                ..StatsBucket::default()
            }
        );
        assert_eq!(st.buckets[2].running, 1);
        assert_eq!(st.buckets[2].p95_us, None, "no finished run, no percentile");
        assert_eq!(
            st.totals.succeeded + st.totals.failed + st.totals.running,
            11
        );
        assert_eq!(st.totals.p95_us, Some(10_000));
        // all apps: the 777 ms run is the slowest
        let all = s.stats(0, 180_000, 60_000, None).unwrap();
        assert_eq!(all.totals.p95_us, Some(777_000));
        assert_eq!(all.buckets[0].succeeded, 10);
    }

    #[test]
    fn stats_falls_back_to_wall_ms_and_rejects_bad_ranges() {
        let s = store();
        old_run(&s, "a", 10, "succeeded", None, Some(3));
        let st = s.stats(0, 1000, 1000, None).unwrap();
        assert_eq!(
            st.totals.p50_us,
            Some(3000),
            "milliseconds become microseconds"
        );
        assert!(s.stats(5, 5, 1000, None).is_err());
        assert!(s.stats(0, 1000, 999, None).is_err());
        assert!(
            s.stats(0, 1_001_000, 1000, None).is_err(),
            "more than 1000 buckets"
        );
        let empty = s.stats(2000, 4000, 1000, None).unwrap();
        assert_eq!(empty.totals.succeeded, 0);
        assert_eq!(empty.totals.p95_us, None);
    }

    #[test]
    fn a_database_without_wall_us_is_migrated() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, app TEXT NOT NULL, deployment INTEGER NOT NULL, trigger TEXT NOT NULL,
               status TEXT NOT NULL, exit_code INTEGER, error TEXT, started_at INTEGER NOT NULL, finished_at INTEGER,
               wall_ms INTEGER, fuel INTEGER, peak_memory INTEGER, host_calls TEXT, trace_id TEXT NOT NULL, message_id INTEGER);
             INSERT INTO runs (id, app, deployment, trigger, status, started_at, wall_ms, trace_id)
               VALUES ('old', 'a', 1, 'api', 'succeeded', 5, 7, 't');",
        )
        .unwrap();
        let s = Store::init(conn).unwrap();
        let r = s.run("old").unwrap();
        assert_eq!(
            (r.wall_ms, r.wall_us),
            (Some(7), None),
            "old rows keep their data"
        );
        s.finish_run(
            &s.start_run("a", 1, "api", None).unwrap().id,
            &RunOutcome {
                status: "succeeded".into(),
                wall_us: Some(42),
                ..RunOutcome::default()
            },
        )
        .unwrap();
        assert_eq!(s.runs(None, 1).unwrap()[0].wall_us, Some(42));
    }
}
