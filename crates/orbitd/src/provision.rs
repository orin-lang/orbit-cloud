//! PostgreSQL provisioning: with `--postgres-url` an app whose spec says `"postgres": true` gets its own database and
//! login role, created idempotently on deploy, and a `DATABASE_URL` in its environment plus a net connect grant to the
//! server. See `docs/postgres.md`.
//!
//! * The operator's URL is a superuser (or, on PostgreSQL 16 and later, `CREATEROLE` + `CREATEDB`) connection; orbitd
//!   connects with it only to provision, inspect and purge. Apps never see it.
//! * Names: database and role are both `oc_<app>` (`-` becomes `_`; apps cannot contain `_`, so the mapping is
//!   injective). A project scope will add a distinct prefix in [`names`]; nothing else knows the format.
//! * Every identifier and literal that reaches SQL goes through [`quote_ident`] / [`quote_literal`]; the statements are
//!   built by pure functions ([`plan_ensure`], [`plan_purge`]) that the unit tests check, including hostile names.
//! * The generated password is stored in the orbitd store in clear text (encryption at rest is a follow-up).
//! * Deleting an app keeps its database; `DELETE /v1/apps/{app}?purge=true` drops database and role.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use oc_store::{AppDatabase, Store, now_ms};
use rand::Rng as _;
use serde_json::{Value, json};
use tokio_postgres::config::Host;
use tokio_postgres::{Client, Config, NoTls};

/// Marks a role as created by orbitd, so provisioning never takes over (or drops) somebody else's role.
const ROLE_COMMENT_PREFIX: &str = "orbit-cloud:app=";
/// Connections one app role may hold.
const CONNECTION_LIMIT: u32 = 25;
const PASSWORD_LEN: usize = 32;

// ------------------------------------------------------------------------------------------------ pure: names, SQL

/// The database and role of an app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    /// Database name (also the role name today).
    pub database: String,
    /// Login role, the owner of the database.
    pub role: String,
}

/// The names for `app` (a valid app name: `a-z0-9-`). PostgreSQL identifiers hold 63 bytes; longer names are cut and
/// get a hash suffix so they stay unique and deterministic.
#[must_use]
pub fn names(app: &str) -> Names {
    use sha2::{Digest as _, Sha256};
    let mut name = format!("oc_{}", app.replace('-', "_"));
    if name.len() > 63 {
        let digest = Sha256::digest(app.as_bytes());
        let mut suffix = String::new();
        for b in &digest[..6] {
            let _ = write!(suffix, "{b:02x}");
        }
        name.truncate(63 - 1 - suffix.len());
        name = format!("{name}_{suffix}");
    }
    Names {
        database: name.clone(),
        role: name,
    }
}

/// An SQL identifier, quoted: `"name"` with embedded quotes doubled.
///
/// # Errors
/// If the text holds a NUL byte, which PostgreSQL cannot represent.
pub fn quote_ident(s: &str) -> anyhow::Result<String> {
    if s.contains('\0') {
        bail!("identifier contains a NUL byte");
    }
    Ok(format!("\"{}\"", s.replace('"', "\"\"")))
}

/// An SQL string literal in escape syntax (`E'...'`), correct whatever `standard_conforming_strings` is: quotes and
/// backslashes are doubled.
///
/// # Errors
/// If the text holds a NUL byte.
pub fn quote_literal(s: &str) -> anyhow::Result<String> {
    if s.contains('\0') {
        bail!("literal contains a NUL byte");
    }
    Ok(format!(
        "E'{}'",
        s.replace('\\', "\\\\").replace('\'', "''")
    ))
}

/// What is already on the server for an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleState {
    /// No such role.
    Missing,
    /// Created by orbitd for this app.
    Managed,
    /// Exists but was not created by orbitd: never touched.
    Foreign,
}

/// The state [`plan_ensure`] starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observed {
    /// The role.
    pub role: RoleState,
    /// Whether the database exists.
    pub database_exists: bool,
    /// Whether the store already holds the role's password (otherwise a new one is set).
    pub password_known: bool,
}

/// The statements that bring the server to the wanted state, in order; empty when nothing is missing. Each is run on its
/// own (`CREATE DATABASE` cannot be in a transaction).
///
/// # Errors
/// If the role exists and is not managed by orbitd, or a name holds a NUL byte.
pub fn plan_ensure(
    n: &Names,
    app: &str,
    password: &str,
    seen: Observed,
) -> anyhow::Result<Vec<String>> {
    let role = quote_ident(&n.role)?;
    let db = quote_ident(&n.database)?;
    let pw = quote_literal(password)?;
    let comment = quote_literal(&format!("{ROLE_COMMENT_PREFIX}{app}"))?;
    let mut out = Vec::new();
    match seen.role {
        RoleState::Foreign => bail!(
            "role `{}` exists but was not created by orbit-cloud; refusing to use it",
            n.role
        ),
        RoleState::Missing => {
            out.push(format!(
                "CREATE ROLE {role} LOGIN PASSWORD {pw} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS \
                 CONNECTION LIMIT {CONNECTION_LIMIT}"
            ));
            out.push(format!("COMMENT ON ROLE {role} IS {comment}"));
        }
        RoleState::Managed if !seen.password_known => {
            out.push(format!("ALTER ROLE {role} WITH LOGIN PASSWORD {pw}"));
        }
        RoleState::Managed => {}
    }
    if !seen.database_exists {
        out.push(format!("CREATE DATABASE {db} OWNER {role}"));
    }
    // Other app roles must not be able to connect to this database (PUBLIC may by default). Idempotent, and repeated
    // whenever provisioning was incomplete so an interrupted first run is repaired.
    if !seen.database_exists || !seen.password_known {
        out.push(format!("REVOKE ALL ON DATABASE {db} FROM PUBLIC"));
    }
    Ok(out)
}

/// The statements that remove an app's database and role. Dropping the database terminates its sessions.
///
/// # Errors
/// If a name holds a NUL byte.
pub fn plan_purge(n: &Names) -> anyhow::Result<Vec<String>> {
    Ok(vec![
        format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_ident(&n.database)?
        ),
        format!("DROP ROLE IF EXISTS {}", quote_ident(&n.role)?),
    ])
}

fn random_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..PASSWORD_LEN)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect()
}

// ------------------------------------------------------------------------------------------------ the server

/// The operator's PostgreSQL server and the connection that administers it.
pub struct Postgres {
    admin: Config,
    host: String,
    port: u16,
    /// Serialises provisioning so two deploys of one app cannot race on `CREATE ROLE`.
    lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Postgres {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Postgres")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Postgres {
    /// Parses the operator's `--postgres-url` (`postgres://user:password@host:port/database`; the database is the one
    /// the admin connection starts in, default `postgres`). Does not connect.
    ///
    /// # Errors
    /// If the URL is malformed or names no TCP host (apps must be able to reach the server, so a socket path is no use).
    pub fn from_url(url: &str) -> anyhow::Result<Self> {
        let mut admin: Config = url.parse().context("bad --postgres-url")?;
        if admin.get_dbname().is_none() {
            admin.dbname("postgres");
        }
        admin.connect_timeout(Duration::from_secs(10));
        let host = match admin.get_hosts() {
            [Host::Tcp(h)] => h.clone(),
            [] => {
                bail!("--postgres-url needs a host (postgres://user:password@host:5432/postgres)")
            }
            _ => bail!("--postgres-url must name exactly one TCP host"),
        };
        let port = admin.get_ports().first().copied().unwrap_or(5432);
        Ok(Self {
            admin,
            host,
            port,
            lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The `HOST:PORT` an app is granted to connect to.
    #[must_use]
    pub fn connect_pattern(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The URL an app uses; the password is included only when asked for.
    #[must_use]
    pub fn database_url(&self, d: &AppDatabase, with_password: bool) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let auth = if with_password {
            format!("{}:{}", d.role, d.password)
        } else {
            d.role.clone()
        };
        format!("postgres://{auth}@{host}:{}/{}", self.port, d.db_name)
    }

    async fn client(&self) -> anyhow::Result<Client> {
        let (client, connection) = self.admin.connect(NoTls).await.with_context(|| {
            format!("cannot connect to PostgreSQL at {}", self.connect_pattern())
        })?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!("postgres admin connection ended: {e}");
            }
        });
        Ok(client)
    }

    async fn observe(
        client: &Client,
        n: &Names,
        app: &str,
        password_known: bool,
    ) -> anyhow::Result<Observed> {
        let role = match client
            .query_opt(
                "SELECT shobj_description(oid, 'pg_authid') FROM pg_roles WHERE rolname = $1",
                &[&n.role],
            )
            .await?
        {
            None => RoleState::Missing,
            Some(row) => {
                let comment: Option<String> = row.get(0);
                if comment.as_deref() == Some(&format!("{ROLE_COMMENT_PREFIX}{app}")) {
                    RoleState::Managed
                } else {
                    RoleState::Foreign
                }
            }
        };
        let database_exists = client
            .query_opt(
                "SELECT 1 FROM pg_database WHERE datname = $1",
                &[&n.database],
            )
            .await?
            .is_some();
        Ok(Observed {
            role,
            database_exists,
            password_known,
        })
    }

    /// Creates the app's database and role if they are missing and returns them. Idempotent: a second call changes
    /// nothing; a role that exists without a stored password (an interrupted first call) gets a fresh one.
    ///
    /// # Errors
    /// Connection and SQL failures; a foreign role of the same name.
    pub async fn ensure(&self, store: &Arc<Store>, app: &str) -> anyhow::Result<AppDatabase> {
        let _serial = self.lock.lock().await;
        let n = names(app);
        let existing = store.app_database(app)?;
        let password = existing
            .as_ref()
            .map_or_else(random_password, |d| d.password.clone());
        let client = self.client().await?;
        let seen = Self::observe(&client, &n, app, existing.is_some()).await?;
        for sql in plan_ensure(&n, app, &password, seen)? {
            client
                .batch_execute(&sql)
                .await
                .with_context(|| format!("provisioning {}: {}", n.database, redact(&sql)))?;
        }
        if let Some(d) = existing {
            return Ok(d);
        }
        let d = AppDatabase {
            app: app.to_owned(),
            db_name: n.database,
            role: n.role,
            password,
            created_at: now_ms(),
        };
        store.put_app_database(&d)?;
        Ok(d)
    }

    /// Drops the app's database and role and forgets its password. False if orbitd manages none for this app.
    ///
    /// # Errors
    /// Connection and SQL failures.
    pub async fn purge(&self, store: &Arc<Store>, app: &str) -> anyhow::Result<bool> {
        let _serial = self.lock.lock().await;
        let n = names(app);
        let client = self.client().await?;
        let seen = Self::observe(&client, &n, app, true).await?;
        if seen.role != RoleState::Managed && store.app_database(app)?.is_none() {
            return Ok(false);
        }
        if seen.role == RoleState::Foreign {
            bail!(
                "role `{}` is not managed by orbit-cloud; not dropping it",
                n.role
            );
        }
        for sql in plan_purge(&n)? {
            client
                .batch_execute(&sql)
                .await
                .with_context(|| sql.clone())?;
        }
        store.delete_app_database(app)?;
        Ok(true)
    }

    /// The status document of `GET /v1/apps/{app}/database`.
    ///
    /// # Errors
    /// Store failures; connection failures are reported inside the document (`reachable: false`), not as an error.
    pub async fn status(
        &self,
        store: &Arc<Store>,
        app: &str,
        with_password: bool,
        enabled: bool,
    ) -> anyhow::Result<Value> {
        let Some(d) = store.app_database(app)? else {
            return Ok(json!({ "app": app, "enabled": enabled, "provisioned": false }));
        };
        let mut v = json!({
            "app": app,
            "enabled": enabled,
            "provisioned": true,
            "database": d.db_name,
            "role": d.role,
            "host": self.host,
            "port": self.port,
            "url": self.database_url(&d, with_password),
            "created_at": d.created_at,
        });
        if with_password {
            v["password"] = json!(d.password);
        }
        match self.client().await {
            Ok(client) => {
                let size: Option<i64> = client
                    .query_opt("SELECT pg_database_size($1::text::name)", &[&d.db_name])
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.get(0));
                let conns: Option<i64> = client
                    .query_opt(
                        "SELECT count(*) FROM pg_stat_activity WHERE datname = $1::text::name",
                        &[&d.db_name],
                    )
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.get(0));
                v["reachable"] = json!(true);
                v["size_bytes"] = json!(size);
                v["connections"] = json!(conns);
            }
            Err(e) => {
                v["reachable"] = json!(false);
                v["error"] = json!(format!("{e:#}"));
            }
        }
        Ok(v)
    }
}

/// A statement for an error message without the password literal.
fn redact(sql: &str) -> String {
    match sql.find("PASSWORD ") {
        Some(i) => format!("{}PASSWORD '***'", &sql[..i]),
        None => sql.to_owned(),
    }
}

// ------------------------------------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn n(app: &str) -> Names {
        names(app)
    }

    #[test]
    fn names_are_prefixed_injective_and_bounded() {
        assert_eq!(n("shop").database, "oc_shop");
        assert_eq!(n("my-app").role, "oc_my_app");
        // `-` and `_` cannot both occur in app names, so different apps never share a name.
        assert_ne!(n("a-b"), n("ab"));
        let long = "a".repeat(63);
        let ln = n(&long);
        assert!(ln.database.len() <= 63, "{}", ln.database.len());
        assert_eq!(ln, n(&long), "deterministic");
        assert_ne!(
            ln,
            n(&format!("{}b", "a".repeat(62))),
            "cut names stay unique"
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("oc_x").unwrap(), "\"oc_x\"");
        assert_eq!(quote_ident("we\"ird").unwrap(), "\"we\"\"ird\"");
        assert_eq!(
            quote_ident("x\"; drop database postgres; --").unwrap(),
            "\"x\"\"; drop database postgres; --\""
        );
        assert!(quote_ident("a\0b").is_err());
        assert_eq!(quote_literal("pw").unwrap(), "E'pw'");
        assert_eq!(quote_literal("it's").unwrap(), "E'it''s'");
        assert_eq!(quote_literal("a\\b").unwrap(), "E'a\\\\b'");
        assert_eq!(
            quote_literal("\\'; drop role x; --").unwrap(),
            "E'\\\\''; drop role x; --'"
        );
        assert!(quote_literal("a\0b").is_err());
    }

    fn seen(role: RoleState, db: bool, known: bool) -> Observed {
        Observed {
            role,
            database_exists: db,
            password_known: known,
        }
    }

    #[test]
    fn first_provisioning_creates_role_comment_database_and_revokes_public() {
        let plan = plan_ensure(
            &n("shop"),
            "shop",
            "s3cret",
            seen(RoleState::Missing, false, false),
        )
        .unwrap();
        assert_eq!(plan.len(), 4, "{plan:?}");
        assert!(
            plan[0].starts_with(
                "CREATE ROLE \"oc_shop\" LOGIN PASSWORD E's3cret' NOSUPERUSER NOCREATEDB"
            ),
            "{}",
            plan[0]
        );
        assert!(plan[0].ends_with("CONNECTION LIMIT 25"));
        assert_eq!(
            plan[1],
            "COMMENT ON ROLE \"oc_shop\" IS E'orbit-cloud:app=shop'"
        );
        assert_eq!(plan[2], "CREATE DATABASE \"oc_shop\" OWNER \"oc_shop\"");
        assert_eq!(plan[3], "REVOKE ALL ON DATABASE \"oc_shop\" FROM PUBLIC");
    }

    #[test]
    fn provisioning_is_idempotent() {
        let done = plan_ensure(
            &n("shop"),
            "shop",
            "pw",
            seen(RoleState::Managed, true, true),
        )
        .unwrap();
        assert!(done.is_empty(), "{done:?}");
    }

    #[test]
    fn interrupted_provisioning_is_completed_with_a_new_password() {
        // The role was created but the store never got the password.
        let plan = plan_ensure(
            &n("shop"),
            "shop",
            "new",
            seen(RoleState::Managed, false, false),
        )
        .unwrap();
        assert_eq!(plan[0], "ALTER ROLE \"oc_shop\" WITH LOGIN PASSWORD E'new'");
        assert_eq!(plan[1], "CREATE DATABASE \"oc_shop\" OWNER \"oc_shop\"");
        // The database is missing but the role and password are known (somebody dropped it).
        let plan = plan_ensure(
            &n("shop"),
            "shop",
            "pw",
            seen(RoleState::Managed, false, true),
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan[0].starts_with("CREATE DATABASE"));
    }

    #[test]
    fn a_foreign_role_is_never_taken_over() {
        let e = plan_ensure(
            &n("shop"),
            "shop",
            "pw",
            seen(RoleState::Foreign, true, false),
        )
        .unwrap_err();
        assert!(e.to_string().contains("not created by orbit-cloud"), "{e}");
    }

    #[test]
    fn hostile_names_and_passwords_stay_inside_their_quotes() {
        let evil = Names {
            database: "x\"; DROP DATABASE postgres; --".into(),
            role: "r\"; ALTER ROLE postgres SUPERUSER; --".into(),
        };
        let plan = plan_ensure(
            &evil,
            "a'b",
            "p'\\; DROP ROLE x; --",
            seen(RoleState::Missing, false, false),
        )
        .unwrap();
        assert_eq!(
            plan[0],
            "CREATE ROLE \"r\"\"; ALTER ROLE postgres SUPERUSER; --\" LOGIN PASSWORD E'p''\\\\; DROP ROLE x; --' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 25"
        );
        assert!(
            plan[1].ends_with("IS E'orbit-cloud:app=a''b'"),
            "{}",
            plan[1]
        );
        assert_eq!(
            plan[2],
            "CREATE DATABASE \"x\"\"; DROP DATABASE postgres; --\" OWNER \"r\"\"; ALTER ROLE postgres SUPERUSER; --\""
        );
        let purge = plan_purge(&evil).unwrap();
        assert_eq!(
            purge[0],
            "DROP DATABASE IF EXISTS \"x\"\"; DROP DATABASE postgres; --\" WITH (FORCE)"
        );
        assert_eq!(
            purge[1],
            "DROP ROLE IF EXISTS \"r\"\"; ALTER ROLE postgres SUPERUSER; --\""
        );
    }

    #[test]
    fn purge_plan() {
        let p = plan_purge(&n("shop")).unwrap();
        assert_eq!(
            p,
            [
                "DROP DATABASE IF EXISTS \"oc_shop\" WITH (FORCE)",
                "DROP ROLE IF EXISTS \"oc_shop\""
            ]
        );
    }

    #[test]
    fn passwords_are_long_and_alphanumeric() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), PASSWORD_LEN);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn url_parsing_and_database_url() {
        let pg = Postgres::from_url("postgres://admin:secret@db.internal:6543/postgres").unwrap();
        assert_eq!(pg.connect_pattern(), "db.internal:6543");
        let d = AppDatabase {
            app: "shop".into(),
            db_name: "oc_shop".into(),
            role: "oc_shop".into(),
            password: "pw".into(),
            created_at: 0,
        };
        assert_eq!(
            pg.database_url(&d, false),
            "postgres://oc_shop@db.internal:6543/oc_shop"
        );
        assert_eq!(
            pg.database_url(&d, true),
            "postgres://oc_shop:pw@db.internal:6543/oc_shop"
        );
        let default_port = Postgres::from_url("postgres://admin@pg/postgres").unwrap();
        assert_eq!(default_port.connect_pattern(), "pg:5432");
        assert!(Postgres::from_url("not a url").is_err());
        assert!(Postgres::from_url("postgres://admin@/postgres").is_err());
    }

    #[test]
    fn errors_never_show_the_password() {
        assert_eq!(
            redact("CREATE ROLE \"r\" LOGIN PASSWORD E'secret' NOSUPERUSER"),
            "CREATE ROLE \"r\" LOGIN PASSWORD '***'"
        );
        assert_eq!(redact("CREATE DATABASE x"), "CREATE DATABASE x");
    }
}
