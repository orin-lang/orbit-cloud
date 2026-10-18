-- Schema version 2: organisations, projects, users, sessions, API tokens, audit log, routes, domains, ports, certs.
-- Applied once to version-1 databases (fresh ones are created at version 1 first). Existing apps move to the
-- `default` project of the `default` organisation; their names (and so their keys) do not change.
BEGIN;

CREATE TABLE orgs (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL REFERENCES orgs(id),
    name       TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

ALTER TABLE apps ADD COLUMN project_id TEXT;
ALTER TABLE apps ADD COLUMN short TEXT;

INSERT INTO orgs (id, name, created_at) VALUES ('org_default', 'default', CAST(strftime('%s','now') AS INTEGER) * 1000);
INSERT INTO projects (id, org_id, name, created_at)
    VALUES ('prj_default', 'org_default', 'default', CAST(strftime('%s','now') AS INTEGER) * 1000);
UPDATE apps SET project_id = 'prj_default', short = name;
CREATE UNIQUE INDEX apps_by_project ON apps (project_id, short);

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    password_hash TEXT,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE memberships (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id  TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    role    TEXT NOT NULL,
    PRIMARY KEY (user_id, org_id)
);

CREATE TABLE sessions (
    id_hash    TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL,
    ip         TEXT,
    user_agent TEXT
);

CREATE TABLE api_tokens (
    id         TEXT PRIMARY KEY,
    hash       TEXT NOT NULL UNIQUE,
    prefix     TEXT NOT NULL,
    name       TEXT NOT NULL,
    user_id    TEXT REFERENCES users(id) ON DELETE CASCADE,
    scope_kind TEXT NOT NULL,
    scope_id   TEXT NOT NULL,
    role       TEXT NOT NULL,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    last_used  INTEGER,
    revoked_at INTEGER
);

CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    actor_kind  TEXT NOT NULL,
    actor_id    TEXT,
    actor_label TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    org_id      TEXT,
    project_id  TEXT,
    detail      TEXT,
    ip          TEXT
);
CREATE INDEX audit_by_org ON audit_log (org_id, id);
CREATE INDEX audit_by_project ON audit_log (project_id, id);

CREATE TABLE routes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    host        TEXT NOT NULL DEFAULT '',
    path_prefix TEXT NOT NULL,
    app         TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (project_id, host, path_prefix)
);

CREATE TABLE domains (
    host         TEXT NOT NULL,
    project_id   TEXT NOT NULL REFERENCES projects(id),
    txt_value    TEXT NOT NULL,
    status       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    verified_at  INTEGER,
    last_checked INTEGER,
    last_error   TEXT,
    cname_ok     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host, project_id)
);
CREATE UNIQUE INDEX domains_one_verified ON domains (host) WHERE status = 'verified';

CREATE TABLE app_ports (
    app  TEXT PRIMARY KEY,
    port INTEGER NOT NULL UNIQUE
);

CREATE TABLE certs (
    host       TEXT PRIMARY KEY,
    cert_pem   TEXT NOT NULL,
    key_pem    TEXT NOT NULL,
    not_after  INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE acme_accounts (
    directory   TEXT PRIMARY KEY,
    credentials TEXT NOT NULL
);

UPDATE meta SET value = '2' WHERE key = 'schema_version';
COMMIT;
