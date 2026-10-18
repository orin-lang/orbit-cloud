-- Orbit Cloud schema, version 1. Times are Unix milliseconds (UTC).

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS apps (
    name              TEXT PRIMARY KEY,
    created_at        INTEGER NOT NULL,
    active_deployment INTEGER
);

-- Modules are content-addressed: one row per distinct .wasm, shared by deployments.
CREATE TABLE IF NOT EXISTS modules (
    sha256     TEXT PRIMARY KEY,
    bytes      BLOB NOT NULL,
    size       INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deployments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    app        TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    version    INTEGER NOT NULL,
    module     TEXT NOT NULL REFERENCES modules(sha256),
    spec       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (app, version)
);

CREATE TABLE IF NOT EXISTS runs (
    id            TEXT PRIMARY KEY,
    app           TEXT NOT NULL,
    deployment    INTEGER NOT NULL,
    trigger       TEXT NOT NULL,
    status        TEXT NOT NULL,
    exit_code     INTEGER,
    error         TEXT,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    wall_ms       INTEGER,
    fuel          INTEGER,
    peak_memory   INTEGER,
    host_calls    TEXT,
    trace_id      TEXT NOT NULL,
    message_id    INTEGER
);
CREATE INDEX IF NOT EXISTS runs_by_app ON runs (app, started_at DESC);

CREATE TABLE IF NOT EXISTS logs (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      INTEGER NOT NULL,
    app     TEXT NOT NULL,
    run_id  TEXT,
    level   TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS logs_by_app ON logs (app, id);
CREATE INDEX IF NOT EXISTS logs_by_run ON logs (run_id, id);

CREATE TABLE IF NOT EXISTS spans (
    trace_id  TEXT NOT NULL,
    span_id   TEXT NOT NULL,
    parent_id TEXT,
    run_id    TEXT NOT NULL,
    name      TEXT NOT NULL,
    start_us  INTEGER NOT NULL,
    end_us    INTEGER NOT NULL,
    attrs     TEXT NOT NULL,
    PRIMARY KEY (trace_id, span_id)
);

CREATE TABLE IF NOT EXISTS kv (
    tenant     TEXT NOT NULL,
    ns         TEXT NOT NULL,
    key        BLOB NOT NULL,
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, ns, key)
);

-- state: ready | leased | done | dead. A leased message whose visible_at has passed is ready again.
CREATE TABLE IF NOT EXISTS queue_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    queue       TEXT NOT NULL,
    body        BLOB NOT NULL,
    state       TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    visible_at  INTEGER NOT NULL,
    enqueued_at INTEGER NOT NULL,
    lease_token TEXT,
    last_error  TEXT
);
CREATE INDEX IF NOT EXISTS queue_ready ON queue_messages (queue, state, visible_at, id);
