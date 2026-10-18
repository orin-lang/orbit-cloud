//! Platform tables: organisations, projects, users, memberships, sessions, API tokens, the audit log, routes,
//! custom domains, HTTP ports and certificates. See `docs/platform.md`.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Result, Store, StoreError, now_ms, random_id, valid_name};

/// Names a project may not take: they are hosts of the platform itself.
pub const RESERVED_PROJECTS: &[&str] = &["console", "www", "api", "acme", "admin", "localhost"];

/// A role in an organisation, or of a token inside its scope. Ordered: `Viewer < Developer < Admin < Owner`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read only.
    Viewer,
    /// Deploy and run.
    Developer,
    /// Manage domains, tokens, members.
    Admin,
    /// Everything, including destructive org operations.
    Owner,
}

impl Role {
    /// The name used in the API and the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Developer => "developer",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    /// Parses a role name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "viewer" => Role::Viewer,
            "developer" => Role::Developer,
            "admin" => Role::Admin,
            "owner" => Role::Owner,
            _ => return None,
        })
    }
}

fn role_col(r: &Row<'_>, i: usize) -> rusqlite::Result<Role> {
    let s: String = r.get(i)?;
    Role::parse(&s)
        .ok_or_else(|| rusqlite::Error::InvalidColumnType(i, s, rusqlite::types::Type::Text))
}

/// An organisation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Org {
    /// Id.
    pub id: String,
    /// Unique name.
    pub name: String,
    /// Creation time.
    pub created_at: i64,
}

/// A project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// Id.
    pub id: String,
    /// Globally unique name (the subdomain).
    pub name: String,
    /// Owning organisation id.
    pub org_id: String,
    /// Owning organisation name.
    pub org: String,
    /// Creation time.
    pub created_at: i64,
}

/// A user.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    /// Id.
    pub id: String,
    /// Lower-case email.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// PHC argon2id string; absent for users that only sign in through an identity provider.
    #[serde(skip)]
    pub password_hash: Option<String>,
    /// May create organisations and users.
    pub is_admin: bool,
    /// Cannot sign in, tokens are rejected.
    pub disabled: bool,
    /// Creation time.
    pub created_at: i64,
}

/// A member of an organisation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    /// The user.
    pub user_id: String,
    /// Their email.
    pub email: String,
    /// Their name.
    pub display_name: String,
    /// Their role.
    pub role: Role,
}

/// A console session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The user.
    pub user_id: String,
    /// Expiry.
    pub expires_at: i64,
    /// Last use.
    pub last_seen: i64,
}

/// What an API token is limited to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    /// All projects of this organisation (id).
    Org(String),
    /// This project (id).
    Project(String),
}

/// An API token record (never the secret).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiToken {
    /// Id.
    pub id: String,
    /// First characters of the secret, to recognise it.
    pub prefix: String,
    /// Label.
    pub name: String,
    /// Creator.
    pub user_id: Option<String>,
    /// `org` or `project`.
    pub scope_kind: String,
    /// Id of the org or project.
    pub scope_id: String,
    /// Role inside the scope.
    pub role: Role,
    /// Expiry.
    pub expires_at: Option<i64>,
    /// Creation time.
    pub created_at: i64,
    /// Last use.
    pub last_used: Option<i64>,
    /// Revocation time.
    pub revoked_at: Option<i64>,
}

impl ApiToken {
    /// The scope.
    #[must_use]
    pub fn scope(&self) -> TokenScope {
        if self.scope_kind == "org" {
            TokenScope::Org(self.scope_id.clone())
        } else {
            TokenScope::Project(self.scope_id.clone())
        }
    }
}

/// An audit log entry to append.
#[derive(Clone, Debug, Default)]
pub struct NewAudit {
    /// `user`, `token`, `admin-token`, `system`.
    pub actor_kind: String,
    /// User or token id.
    pub actor_id: Option<String>,
    /// Human-readable actor (email, token name).
    pub actor_label: String,
    /// `project.create`, `app.deploy`, ...
    pub action: String,
    /// What it was done to.
    pub target: Option<String>,
    /// Organisation id.
    pub org_id: Option<String>,
    /// Project id.
    pub project_id: Option<String>,
    /// Details.
    pub detail: Option<serde_json::Value>,
    /// Client address.
    pub ip: Option<String>,
}

/// A stored audit log entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    /// Cursor.
    pub id: i64,
    /// Time.
    pub ts: i64,
    /// `user`, `token`, `admin-token`, `system`.
    pub actor_kind: String,
    /// User or token id.
    pub actor_id: Option<String>,
    /// Human-readable actor.
    pub actor_label: String,
    /// What happened.
    pub action: String,
    /// What it happened to.
    pub target: Option<String>,
    /// Organisation id.
    pub org_id: Option<String>,
    /// Project id.
    pub project_id: Option<String>,
    /// Details.
    pub detail: Option<serde_json::Value>,
    /// Client address.
    pub ip: Option<String>,
}

/// A route of a project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    /// Id.
    pub id: i64,
    /// Project name.
    pub project: String,
    /// Host this route is limited to; empty = every host of the project.
    pub host: String,
    /// Path prefix, starting with `/`.
    pub path_prefix: String,
    /// App key.
    pub app: String,
}

/// A custom domain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Domain {
    /// Host name.
    pub host: String,
    /// Project name.
    pub project: String,
    /// Value of the verification TXT record.
    pub txt_value: String,
    /// `pending` or `verified`.
    pub status: String,
    /// Creation time.
    pub created_at: i64,
    /// Verification time.
    pub verified_at: Option<i64>,
    /// Last DNS check.
    pub last_checked: Option<i64>,
    /// Why the last check failed.
    pub last_error: Option<String>,
    /// Whether the host pointed at the project host at the last check.
    pub cname_ok: bool,
}

/// A stored certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertRow {
    /// Host.
    pub host: String,
    /// Chain, PEM.
    pub cert_pem: String,
    /// Private key, PEM.
    pub key_pem: String,
    /// Expiry (Unix ms).
    pub not_after: i64,
}

/// Everything the gateway routes with, read in one go.
#[derive(Clone, Debug, Default)]
pub struct GatewaySnapshot {
    /// The generation it was read at.
    pub generation: u64,
    /// Project names.
    pub projects: Vec<String>,
    /// `(host, project)` of verified custom domains.
    pub domains: Vec<(String, String)>,
    /// All routes.
    pub routes: Vec<Route>,
    /// `(app key, port)`.
    pub ports: Vec<(String, u16)>,
}

/// Loopback ports handed to HTTP services.
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = 20000..=29999;

fn project_from_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        org_id: r.get(2)?,
        org: r.get(3)?,
        created_at: r.get(4)?,
    })
}

const PROJECT_SELECT: &str = "SELECT p.id, p.name, p.org_id, o.name, p.created_at FROM projects p JOIN orgs o ON o.id = p.org_id";

fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        display_name: r.get(2)?,
        password_hash: r.get(3)?,
        is_admin: r.get::<_, i64>(4)? != 0,
        disabled: r.get::<_, i64>(5)? != 0,
        created_at: r.get(6)?,
    })
}

const USER_SELECT: &str =
    "SELECT id, email, display_name, password_hash, is_admin, disabled, created_at FROM users";

fn token_from_row(r: &Row<'_>) -> rusqlite::Result<ApiToken> {
    Ok(ApiToken {
        id: r.get(0)?,
        prefix: r.get(1)?,
        name: r.get(2)?,
        user_id: r.get(3)?,
        scope_kind: r.get(4)?,
        scope_id: r.get(5)?,
        role: role_col(r, 6)?,
        expires_at: r.get(7)?,
        created_at: r.get(8)?,
        last_used: r.get(9)?,
        revoked_at: r.get(10)?,
    })
}

const TOKEN_SELECT: &str = "SELECT id, prefix, name, user_id, scope_kind, scope_id, role, expires_at, created_at, last_used, revoked_at FROM api_tokens";

fn route_from_row(r: &Row<'_>) -> rusqlite::Result<Route> {
    Ok(Route {
        id: r.get(0)?,
        project: r.get(1)?,
        host: r.get(2)?,
        path_prefix: r.get(3)?,
        app: r.get(4)?,
    })
}

const ROUTE_SELECT: &str = "SELECT r.id, p.name, r.host, r.path_prefix, r.app FROM routes r JOIN projects p ON p.id = r.project_id";

fn domain_from_row(r: &Row<'_>) -> rusqlite::Result<Domain> {
    Ok(Domain {
        host: r.get(0)?,
        project: r.get(1)?,
        txt_value: r.get(2)?,
        status: r.get(3)?,
        created_at: r.get(4)?,
        verified_at: r.get(5)?,
        last_checked: r.get(6)?,
        last_error: r.get(7)?,
        cname_ok: r.get::<_, i64>(8)? != 0,
    })
}

const DOMAIN_SELECT: &str = "SELECT d.host, p.name, d.txt_value, d.status, d.created_at, d.verified_at, d.last_checked, d.last_error, d.cname_ok FROM domains d JOIN projects p ON p.id = d.project_id";

fn audit_from_row(r: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    let detail: Option<String> = r.get(9)?;
    Ok(AuditEntry {
        id: r.get(0)?,
        ts: r.get(1)?,
        actor_kind: r.get(2)?,
        actor_id: r.get(3)?,
        actor_label: r.get(4)?,
        action: r.get(5)?,
        target: r.get(6)?,
        org_id: r.get(7)?,
        project_id: r.get(8)?,
        detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
        ip: r.get(10)?,
    })
}

/// SHA-256 of `s`, hex: how session ids and API tokens are stored.
#[must_use]
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn conflict<T>(msg: impl Into<String>) -> Result<T> {
    Err(StoreError::Conflict(msg.into()))
}

impl Store {
    // ------------------------------------------------------------------------------------------------ orgs

    /// Creates an organisation.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad or taken name.
    pub fn create_org(&self, name: &str) -> Result<Org> {
        if !valid_name(name) {
            return conflict(
                "invalid organisation name: 1-63 of a-z, 0-9 and -, not starting with -",
            );
        }
        let org = Org {
            id: format!("org_{}", random_id(6)),
            name: name.to_owned(),
            created_at: now_ms(),
        };
        let n = self.db().execute(
            "INSERT OR IGNORE INTO orgs (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![org.id, org.name, org.created_at],
        )?;
        if n == 0 {
            return conflict(format!("organisation `{name}` exists"));
        }
        Ok(org)
    }

    /// An organisation by name.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn org(&self, name: &str) -> Result<Org> {
        self.db()
            .query_row(
                "SELECT id, name, created_at FROM orgs WHERE name = ?1",
                params![name],
                |r| {
                    Ok(Org {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        created_at: r.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("organisation `{name}`")))
    }

    /// All organisations.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn orgs(&self) -> Result<Vec<Org>> {
        let db = self.db();
        let mut st = db.prepare("SELECT id, name, created_at FROM orgs ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(Org {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes an organisation without projects, with its memberships and tokens.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] while it has projects, [`StoreError::NotFound`].
    pub fn delete_org(&self, name: &str) -> Result<()> {
        let org = self.org(name)?;
        let mut db = self.db();
        let tx = db.transaction()?;
        let projects: i64 = tx.query_row(
            "SELECT COUNT(*) FROM projects WHERE org_id = ?1",
            params![org.id],
            |r| r.get(0),
        )?;
        if projects > 0 {
            return conflict(format!(
                "organisation `{name}` still has {projects} project(s)"
            ));
        }
        tx.execute(
            "DELETE FROM api_tokens WHERE scope_kind = 'org' AND scope_id = ?1",
            params![org.id],
        )?;
        tx.execute("DELETE FROM orgs WHERE id = ?1", params![org.id])?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ projects

    /// Creates a project in organisation `org`.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad, reserved or taken name, [`StoreError::NotFound`] for an unknown org.
    pub fn create_project(&self, org: &str, name: &str) -> Result<Project> {
        if !valid_name(name) {
            return conflict("invalid project name: 1-63 of a-z, 0-9 and -, not starting with -");
        }
        if RESERVED_PROJECTS.contains(&name) {
            return conflict(format!("project name `{name}` is reserved"));
        }
        let org = self.org(org)?;
        let id = format!("prj_{}", random_id(6));
        let now = now_ms();
        let n = self.db().execute(
            "INSERT OR IGNORE INTO projects (id, org_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, org.id, name, now],
        )?;
        if n == 0 {
            return conflict(format!("project `{name}` exists"));
        }
        self.bump();
        Ok(Project {
            id,
            name: name.to_owned(),
            org_id: org.id,
            org: org.name,
            created_at: now,
        })
    }

    /// A project by name.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn project(&self, name: &str) -> Result<Project> {
        self.db()
            .query_row(
                &format!("{PROJECT_SELECT} WHERE p.name = ?1"),
                params![name],
                project_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("project `{name}`")))
    }

    /// All projects, or those of one organisation.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn projects(&self, org: Option<&str>) -> Result<Vec<Project>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "{PROJECT_SELECT} WHERE (?1 IS NULL OR o.name = ?1) ORDER BY p.name"
        ))?;
        let rows = st.query_map(params![org], project_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The project that owns the app with key `app`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn project_of_app(&self, app: &str) -> Result<Project> {
        let (p, _) = super::split_key(app);
        self.project(p)
    }

    /// Deletes a project that has no apps, with its routes, domains and tokens.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] while it has apps or is `default`, [`StoreError::NotFound`].
    pub fn delete_project(&self, name: &str) -> Result<()> {
        if name == super::DEFAULT_PROJECT {
            return conflict("the default project cannot be deleted");
        }
        let p = self.project(name)?;
        let mut db = self.db();
        let tx = db.transaction()?;
        let apps: i64 = tx.query_row(
            "SELECT COUNT(*) FROM apps WHERE project_id = ?1",
            params![p.id],
            |r| r.get(0),
        )?;
        if apps > 0 {
            return conflict(format!("project `{name}` still has {apps} app(s)"));
        }
        tx.execute("DELETE FROM routes WHERE project_id = ?1", params![p.id])?;
        tx.execute("DELETE FROM domains WHERE project_id = ?1", params![p.id])?;
        tx.execute(
            "DELETE FROM api_tokens WHERE scope_kind = 'project' AND scope_id = ?1",
            params![p.id],
        )?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![p.id])?;
        tx.commit()?;
        drop(db);
        self.bump();
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ users

    /// The number of users (0 means the node still needs its setup).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn user_count(&self) -> Result<i64> {
        Ok(self
            .db()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    /// Creates a user.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a bad or taken email.
    pub fn create_user(
        &self,
        email: &str,
        display_name: &str,
        password_hash: Option<&str>,
        is_admin: bool,
    ) -> Result<User> {
        let email = email.trim().to_lowercase();
        if email.len() > 254
            || !email.contains('@')
            || email.starts_with('@')
            || email.ends_with('@')
        {
            return conflict("invalid email address");
        }
        let u = User {
            id: format!("usr_{}", random_id(8)),
            email,
            display_name: display_name.trim().chars().take(100).collect(),
            password_hash: password_hash.map(str::to_owned),
            is_admin,
            disabled: false,
            created_at: now_ms(),
        };
        let n = self.db().execute(
            "INSERT OR IGNORE INTO users (id, email, display_name, password_hash, is_admin, disabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![u.id, u.email, u.display_name, u.password_hash, i64::from(is_admin), u.created_at],
        )?;
        if n == 0 {
            return conflict(format!("a user with email `{}` exists", u.email));
        }
        Ok(u)
    }

    /// A user by email (case-insensitive).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self
            .db()
            .query_row(
                &format!("{USER_SELECT} WHERE email = ?1"),
                params![email.trim().to_lowercase()],
                user_from_row,
            )
            .optional()?)
    }

    /// A user by id.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn user(&self, id: &str) -> Result<User> {
        self.db()
            .query_row(
                &format!("{USER_SELECT} WHERE id = ?1"),
                params![id],
                user_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("user {id}")))
    }

    /// All users.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn users(&self) -> Result<Vec<User>> {
        let db = self.db();
        let mut st = db.prepare(&format!("{USER_SELECT} ORDER BY email"))?;
        let rows = st.query_map([], user_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Creates the first user and their ownership of the `default` organisation, atomically; fails when any user
    /// exists (so two setup requests cannot both win).
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when a user exists already or the email is bad.
    pub fn create_first_user(
        &self,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<User> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n: i64 = tx.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        if n > 0 {
            return conflict("setup is done: a user exists already");
        }
        let email = email.trim().to_lowercase();
        if email.len() > 254
            || !email.contains('@')
            || email.starts_with('@')
            || email.ends_with('@')
        {
            return conflict("invalid email address");
        }
        let u = User {
            id: format!("usr_{}", random_id(8)),
            email,
            display_name: display_name.trim().chars().take(100).collect(),
            password_hash: Some(password_hash.to_owned()),
            is_admin: true,
            disabled: false,
            created_at: now_ms(),
        };
        tx.execute(
            "INSERT INTO users (id, email, display_name, password_hash, is_admin, disabled, created_at)
             VALUES (?1, ?2, ?3, ?4, 1, 0, ?5)",
            params![u.id, u.email, u.display_name, u.password_hash, u.created_at],
        )?;
        tx.execute(
            "INSERT INTO memberships (user_id, org_id, role) VALUES (?1, 'org_default', 'owner')",
            params![u.id],
        )?;
        tx.commit()?;
        Ok(u)
    }

    /// Sets a user's password hash.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn set_password(&self, user_id: &str, hash: &str) -> Result<()> {
        let n = self.db().execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            params![user_id, hash],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("user {user_id}")));
        }
        Ok(())
    }

    /// Disables or enables a user; disabling also ends their sessions.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn set_user_disabled(&self, user_id: &str, disabled: bool) -> Result<()> {
        let db = self.db();
        let n = db.execute(
            "UPDATE users SET disabled = ?2 WHERE id = ?1",
            params![user_id, i64::from(disabled)],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("user {user_id}")));
        }
        if disabled {
            db.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ members

    /// Adds or changes a membership.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown org.
    pub fn set_member(&self, org_id: &str, user_id: &str, role: Role) -> Result<()> {
        self.db().execute(
            "INSERT INTO memberships (user_id, org_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, org_id) DO UPDATE SET role = excluded.role",
            params![user_id, org_id, role.as_str()],
        )?;
        Ok(())
    }

    /// Removes a membership. Returns whether there was one.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn remove_member(&self, org_id: &str, user_id: &str) -> Result<bool> {
        Ok(self.db().execute(
            "DELETE FROM memberships WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user_id],
        )? > 0)
    }

    /// The role of a user in an organisation.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn role_of(&self, user_id: &str, org_id: &str) -> Result<Option<Role>> {
        Ok(self
            .db()
            .query_row(
                "SELECT role FROM memberships WHERE user_id = ?1 AND org_id = ?2",
                params![user_id, org_id],
                |r| role_col(r, 0),
            )
            .optional()?)
    }

    /// Members of an organisation.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn members(&self, org_id: &str) -> Result<Vec<Member>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT u.id, u.email, u.display_name, m.role FROM memberships m JOIN users u ON u.id = m.user_id
             WHERE m.org_id = ?1 ORDER BY u.email",
        )?;
        let rows = st.query_map(params![org_id], |r| {
            Ok(Member {
                user_id: r.get(0)?,
                email: r.get(1)?,
                display_name: r.get(2)?,
                role: role_col(r, 3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The organisations a user belongs to, with their role.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn memberships_of(&self, user_id: &str) -> Result<Vec<(Org, Role)>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT o.id, o.name, o.created_at, m.role FROM memberships m JOIN orgs o ON o.id = m.org_id
             WHERE m.user_id = ?1 ORDER BY o.name",
        )?;
        let rows = st.query_map(params![user_id], |r| {
            Ok((
                Org {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    created_at: r.get(2)?,
                },
                role_col(r, 3)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Number of owners of an organisation.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn owner_count(&self, org_id: &str) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM memberships WHERE org_id = ?1 AND role = 'owner'",
            params![org_id],
            |r| r.get(0),
        )?)
    }

    // ------------------------------------------------------------------------------------------------ sessions

    /// Stores a session; `id_hash` is the sha-256 of the cookie value.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn create_session(
        &self,
        id_hash: &str,
        user_id: &str,
        ttl_ms: i64,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let ua: Option<String> = user_agent.map(|u| u.chars().take(200).collect());
        self.db().execute(
            "INSERT INTO sessions (id_hash, user_id, created_at, expires_at, last_seen, ip, user_agent)
             VALUES (?1, ?2, ?3, ?4, ?3, ?5, ?6)",
            params![id_hash, user_id, now, now.saturating_add(ttl_ms), ip, ua],
        )?;
        Ok(())
    }

    /// A live session (not expired, user not disabled), extending it when less than half of `ttl_ms` is left.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn session(&self, id_hash: &str, ttl_ms: i64) -> Result<Option<Session>> {
        let now = now_ms();
        let db = self.db();
        let found = db
            .query_row(
                "SELECT s.user_id, s.expires_at, s.last_seen FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.id_hash = ?1 AND s.expires_at > ?2 AND u.disabled = 0",
                params![id_hash, now],
                |r| {
                    Ok(Session {
                        user_id: r.get(0)?,
                        expires_at: r.get(1)?,
                        last_seen: r.get(2)?,
                    })
                },
            )
            .optional()?;
        if let Some(s) = &found
            && s.expires_at - now < ttl_ms / 2
        {
            db.execute(
                "UPDATE sessions SET expires_at = ?2, last_seen = ?3 WHERE id_hash = ?1",
                params![id_hash, now.saturating_add(ttl_ms), now],
            )?;
        }
        Ok(found)
    }

    /// Deletes a session.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn delete_session(&self, id_hash: &str) -> Result<()> {
        self.db()
            .execute("DELETE FROM sessions WHERE id_hash = ?1", params![id_hash])?;
        Ok(())
    }

    /// Deletes expired sessions. Returns how many.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn purge_sessions(&self) -> Result<usize> {
        Ok(self.db().execute(
            "DELETE FROM sessions WHERE expires_at <= ?1",
            params![now_ms()],
        )?)
    }

    // ------------------------------------------------------------------------------------------------ tokens

    /// Stores an API token. `secret_hash` is the sha-256 of the secret; `prefix` its first characters.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_token(
        &self,
        name: &str,
        prefix: &str,
        secret_hash: &str,
        user_id: Option<&str>,
        scope: &TokenScope,
        role: Role,
        expires_at: Option<i64>,
    ) -> Result<ApiToken> {
        let (kind, scope_id) = match scope {
            TokenScope::Org(id) => ("org", id),
            TokenScope::Project(id) => ("project", id),
        };
        let t = ApiToken {
            id: format!("tok_{}", random_id(6)),
            prefix: prefix.to_owned(),
            name: name.chars().take(100).collect(),
            user_id: user_id.map(str::to_owned),
            scope_kind: kind.to_owned(),
            scope_id: scope_id.clone(),
            role,
            expires_at,
            created_at: now_ms(),
            last_used: None,
            revoked_at: None,
        };
        self.db().execute(
            "INSERT INTO api_tokens (id, hash, prefix, name, user_id, scope_kind, scope_id, role, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                t.id,
                secret_hash,
                t.prefix,
                t.name,
                t.user_id,
                t.scope_kind,
                t.scope_id,
                role.as_str(),
                t.expires_at,
                t.created_at
            ],
        )?;
        Ok(t)
    }

    /// The usable token with this secret hash: not revoked, not expired, creator not disabled. Records the use.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn token_by_hash(&self, secret_hash: &str) -> Result<Option<ApiToken>> {
        let now = now_ms();
        let db = self.db();
        let t = db
            .query_row(
                &format!(
                    "{TOKEN_SELECT} WHERE hash = ?1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2)
                     AND (user_id IS NULL OR EXISTS (SELECT 1 FROM users u WHERE u.id = api_tokens.user_id AND u.disabled = 0))"
                ),
                params![secret_hash, now],
                token_from_row,
            )
            .optional()?;
        if let Some(t) = &t
            && t.last_used.is_none_or(|l| now - l > 60_000)
        {
            db.execute(
                "UPDATE api_tokens SET last_used = ?2 WHERE id = ?1",
                params![t.id, now],
            )?;
        }
        Ok(t)
    }

    /// Tokens, newest first; of one user when `user_id` is given.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn tokens(&self, user_id: Option<&str>) -> Result<Vec<ApiToken>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "{TOKEN_SELECT} WHERE (?1 IS NULL OR user_id = ?1) ORDER BY created_at DESC"
        ))?;
        let rows = st.query_map(params![user_id], token_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One token record.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn token(&self, id: &str) -> Result<ApiToken> {
        self.db()
            .query_row(
                &format!("{TOKEN_SELECT} WHERE id = ?1"),
                params![id],
                token_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("token {id}")))
    }

    /// Revokes a token.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn revoke_token(&self, id: &str) -> Result<()> {
        let n = self.db().execute(
            "UPDATE api_tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("token {id}")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ audit

    /// Appends an audit entry.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn audit(&self, a: &NewAudit) -> Result<()> {
        self.db().execute(
            "INSERT INTO audit_log (ts, actor_kind, actor_id, actor_label, action, target, org_id, project_id, detail, ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                now_ms(),
                a.actor_kind,
                a.actor_id,
                a.actor_label,
                a.action,
                a.target,
                a.org_id,
                a.project_id,
                a.detail.as_ref().map(serde_json::Value::to_string),
                a.ip
            ],
        )?;
        Ok(())
    }

    /// Audit entries, newest first. `org_id` / `project_id` filter; `before` is an exclusive id cursor.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn audit_log(
        &self,
        org_id: Option<&str>,
        project_id: Option<&str>,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<AuditEntry>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT id, ts, actor_kind, actor_id, actor_label, action, target, org_id, project_id, detail, ip
             FROM audit_log WHERE (?1 IS NULL OR org_id = ?1) AND (?2 IS NULL OR project_id = ?2)
               AND (?3 IS NULL OR id < ?3) ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = st.query_map(
            params![org_id, project_id, before, limit.clamp(1, 1000)],
            audit_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------------------------------------------ routes

    /// Adds a route to a project.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] for a duplicate or a bad prefix, [`StoreError::NotFound`].
    pub fn add_route(
        &self,
        project: &str,
        host: &str,
        path_prefix: &str,
        app: &str,
    ) -> Result<Route> {
        if !path_prefix.starts_with('/') || path_prefix.contains(['?', '#', ' ']) {
            return conflict("path_prefix must start with / and contain no ?, # or spaces");
        }
        let p = self.project(project)?;
        let a = self.app(app)?;
        if a.project != project {
            return conflict(format!("app `{app}` is not in project `{project}`"));
        }
        let db = self.db();
        let n = db.execute(
            "INSERT OR IGNORE INTO routes (project_id, host, path_prefix, app, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![p.id, host, path_prefix, app, now_ms()],
        )?;
        if n == 0 {
            return conflict("a route with this host and path prefix exists");
        }
        let id = db.last_insert_rowid();
        drop(db);
        self.bump();
        Ok(Route {
            id,
            project: project.to_owned(),
            host: host.to_owned(),
            path_prefix: path_prefix.to_owned(),
            app: app.to_owned(),
        })
    }

    /// The routes of a project.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn routes(&self, project: &str) -> Result<Vec<Route>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "{ROUTE_SELECT} WHERE p.name = ?1 ORDER BY r.host, length(r.path_prefix) DESC, r.id"
        ))?;
        let rows = st.query_map(params![project], route_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes a route of a project.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn delete_route(&self, project: &str, id: i64) -> Result<()> {
        let n = self.db().execute(
            "DELETE FROM routes WHERE id = ?1 AND project_id = (SELECT id FROM projects WHERE name = ?2)",
            params![id, project],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("route {id}")));
        }
        self.bump();
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ domains

    /// Adds a custom domain claim to a project (status `pending`) with a fresh verification value.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the project claims it already or another project has it verified.
    pub fn add_domain(&self, project: &str, host: &str) -> Result<Domain> {
        let p = self.project(project)?;
        let txt_value = format!("orbit-verify={}", random_id(16));
        let now = now_ms();
        let db = self.db();
        let taken: bool = db
            .query_row(
                "SELECT 1 FROM domains WHERE host = ?1 AND status = 'verified'",
                params![host],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if taken {
            return conflict(format!("`{host}` is verified for a project already"));
        }
        let n = db.execute(
            "INSERT OR IGNORE INTO domains (host, project_id, txt_value, status, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
            params![host, p.id, txt_value, now],
        )?;
        if n == 0 {
            return conflict(format!("project `{project}` has `{host}` already"));
        }
        drop(db);
        self.bump();
        self.domain(project, host)
    }

    /// One domain of a project.
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn domain(&self, project: &str, host: &str) -> Result<Domain> {
        self.db()
            .query_row(
                &format!("{DOMAIN_SELECT} WHERE p.name = ?1 AND d.host = ?2"),
                params![project, host],
                domain_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("domain `{host}`")))
    }

    /// The domains of a project.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn domains(&self, project: &str) -> Result<Vec<Domain>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "{DOMAIN_SELECT} WHERE p.name = ?1 ORDER BY d.host"
        ))?;
        let rows = st.query_map(params![project], domain_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Domains waiting for verification, all projects.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn pending_domains(&self) -> Result<Vec<Domain>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "{DOMAIN_SELECT} WHERE d.status = 'pending' ORDER BY d.created_at"
        ))?;
        let rows = st.query_map([], domain_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The project a verified custom domain belongs to.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn verified_domain_project(&self, host: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT p.name FROM domains d JOIN projects p ON p.id = d.project_id WHERE d.host = ?1 AND d.status = 'verified'",
                params![host],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Records a DNS check. `verified` marks the domain verified (and drops the pending claims of other projects).
    ///
    /// # Errors
    /// [`StoreError::NotFound`], [`StoreError::Conflict`] when another project verified the host meanwhile.
    pub fn record_domain_check(
        &self,
        project: &str,
        host: &str,
        verified: bool,
        cname_ok: bool,
        error: Option<&str>,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let pid: String = tx
            .query_row(
                "SELECT id FROM projects WHERE name = ?1",
                params![project],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("project `{project}`")))?;
        let now = now_ms();
        let n = tx.execute(
            "UPDATE domains SET last_checked = ?3, last_error = ?4, cname_ok = ?5 WHERE host = ?1 AND project_id = ?2",
            params![host, pid, now, error, i64::from(cname_ok)],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("domain `{host}`")));
        }
        if verified {
            let res = tx.execute(
                "UPDATE domains SET status = 'verified', verified_at = ?3, last_error = NULL
                 WHERE host = ?1 AND project_id = ?2 AND status = 'pending'",
                params![host, pid, now],
            );
            match res {
                Ok(_) => {
                    tx.execute(
                        "DELETE FROM domains WHERE host = ?1 AND project_id <> ?2 AND status = 'pending'",
                        params![host, pid],
                    )?;
                }
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return conflict(format!("`{host}` is verified for another project"));
                }
                Err(e) => return Err(e.into()),
            }
        }
        tx.commit()?;
        drop(db);
        self.bump();
        Ok(())
    }

    /// Removes a domain claim and its certificate (if no other project still has the host verified).
    ///
    /// # Errors
    /// [`StoreError::NotFound`].
    pub fn delete_domain(&self, project: &str, host: &str) -> Result<()> {
        let db = self.db();
        let n = db.execute(
            "DELETE FROM domains WHERE host = ?1 AND project_id = (SELECT id FROM projects WHERE name = ?2)",
            params![host, project],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("domain `{host}`")));
        }
        db.execute("DELETE FROM routes WHERE host = ?1 AND project_id = (SELECT id FROM projects WHERE name = ?2)", params![host, project])?;
        let still: bool = db
            .query_row(
                "SELECT 1 FROM domains WHERE host = ?1",
                params![host],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !still {
            db.execute("DELETE FROM certs WHERE host = ?1", params![host])?;
        }
        drop(db);
        self.bump();
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------ ports

    /// The loopback port of an HTTP service, assigned on first use and kept.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the range is exhausted.
    pub fn http_port(&self, app: &str) -> Result<u16> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(p) = tx
            .query_row(
                "SELECT port FROM app_ports WHERE app = ?1",
                params![app],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            return u16::try_from(p).map_err(|_| StoreError::Conflict("bad stored port".into()));
        }
        let used: std::collections::HashSet<u16> = {
            let mut st = tx.prepare("SELECT port FROM app_ports")?;
            let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
            rows.filter_map(|p| p.ok().and_then(|p| u16::try_from(p).ok()))
                .collect()
        };
        let Some(port) = PORT_RANGE.clone().find(|p| !used.contains(p)) else {
            return conflict("no free loopback ports left for HTTP services");
        };
        tx.execute(
            "INSERT INTO app_ports (app, port) VALUES (?1, ?2)",
            params![app, i64::from(port)],
        )?;
        tx.commit()?;
        drop(db);
        self.bump();
        Ok(port)
    }

    /// Pins the loopback port of an app (tests, manual operation). Replaces the app's current port.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when another app has the port.
    pub fn assign_http_port(&self, app: &str, port: u16) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM app_ports WHERE app = ?1", params![app])?;
        let n = db.execute(
            "INSERT OR IGNORE INTO app_ports (app, port) VALUES (?1, ?2)",
            params![app, i64::from(port)],
        )?;
        drop(db);
        self.bump();
        if n == 0 {
            return conflict(format!("port {port} belongs to another app"));
        }
        Ok(())
    }

    /// The port of an app, when it has one.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn app_port(&self, app: &str) -> Result<Option<u16>> {
        Ok(self
            .db()
            .query_row(
                "SELECT port FROM app_ports WHERE app = ?1",
                params![app],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .and_then(|p| u16::try_from(p).ok()))
    }

    // ------------------------------------------------------------------------------------------------ gateway

    /// Everything the gateway routes with.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn gateway_snapshot(&self) -> Result<GatewaySnapshot> {
        let generation = self.generation();
        let db = self.db();
        let projects = {
            let mut st = db.prepare("SELECT name FROM projects")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let domains = {
            let mut st = db.prepare(
                "SELECT d.host, p.name FROM domains d JOIN projects p ON p.id = d.project_id WHERE d.status = 'verified'",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let routes = {
            let mut st = db.prepare(ROUTE_SELECT)?;
            let rows = st.query_map([], route_from_row)?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let ports = {
            let mut st = db.prepare("SELECT app, port FROM app_ports")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.filter_map(std::result::Result::ok)
                .filter_map(|(a, p)| u16::try_from(p).ok().map(|p| (a, p)))
                .collect()
        };
        Ok(GatewaySnapshot {
            generation,
            projects,
            domains,
            routes,
            ports,
        })
    }

    /// The id of the newest running run of an app and its trace id (to attach request spans to).
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn running_run(&self, app: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, trace_id FROM runs WHERE app = ?1 AND status = 'running' ORDER BY started_at DESC LIMIT 1",
                params![app],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    // ------------------------------------------------------------------------------------------------ certs

    /// Stores (replaces) the certificate of a host.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn put_cert(
        &self,
        host: &str,
        cert_pem: &str,
        key_pem: &str,
        not_after: i64,
    ) -> Result<()> {
        self.db().execute(
            "INSERT INTO certs (host, cert_pem, key_pem, not_after, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (host) DO UPDATE SET cert_pem = excluded.cert_pem, key_pem = excluded.key_pem,
               not_after = excluded.not_after, created_at = excluded.created_at",
            params![host, cert_pem, key_pem, not_after, now_ms()],
        )?;
        Ok(())
    }

    /// The stored certificate of a host.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn cert(&self, host: &str) -> Result<Option<CertRow>> {
        Ok(self
            .db()
            .query_row(
                "SELECT host, cert_pem, key_pem, not_after FROM certs WHERE host = ?1",
                params![host],
                |r| {
                    Ok(CertRow {
                        host: r.get(0)?,
                        cert_pem: r.get(1)?,
                        key_pem: r.get(2)?,
                        not_after: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Hosts whose certificate expires before `before_ms`.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn certs_expiring(&self, before_ms: i64) -> Result<Vec<String>> {
        let db = self.db();
        let mut st = db.prepare("SELECT host FROM certs WHERE not_after < ?1")?;
        let rows = st.query_map(params![before_ms], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The ACME account credentials stored for a directory.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn acme_account(&self, directory: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT credentials FROM acme_accounts WHERE directory = ?1",
                params![directory],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stores the ACME account credentials of a directory.
    ///
    /// # Errors
    /// [`StoreError::Db`].
    pub fn put_acme_account(&self, directory: &str, credentials: &str) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO acme_accounts (directory, credentials) VALUES (?1, ?2)",
            params![directory, credentials],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn migration_creates_default_org_and_project() {
        let s = store();
        assert_eq!(s.org("default").unwrap().id, "org_default");
        assert_eq!(s.project("default").unwrap().org, "default");
        let m = s.put_module(b"\0asm").unwrap();
        s.deploy("hello", &m, &serde_json::json!({})).unwrap();
        let a = s.app("hello").unwrap();
        assert_eq!((a.project.as_str(), a.short.as_str()), ("default", "hello"));
    }

    #[test]
    fn old_database_is_migrated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            // a version-1 database with an app in it
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(include_str!("schema.sql")).unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version', '1')", [])
                .unwrap();
            c.execute(
                "INSERT INTO apps (name, created_at) VALUES ('legacy', 5)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let a = s.app("legacy").unwrap();
        assert_eq!(
            (a.project.as_str(), a.short.as_str()),
            ("default", "legacy")
        );
        assert_eq!(s.apps_in("default").unwrap().len(), 1);
        drop(s);
        Store::open(&path).unwrap(); // idempotent
    }

    #[test]
    fn projects_are_unique_and_scope_apps() {
        let s = store();
        s.create_org("acme").unwrap();
        s.create_project("acme", "shop").unwrap();
        assert!(
            s.create_project("default", "shop").is_err(),
            "globally unique"
        );
        assert!(s.create_project("acme", "console").is_err(), "reserved");
        assert!(s.create_project("acme", "Bad_Name").is_err());
        let m = s.put_module(b"\0asm").unwrap();
        s.deploy("shop.web", &m, &serde_json::json!({})).unwrap();
        s.deploy("web", &m, &serde_json::json!({})).unwrap();
        assert_eq!(s.app("shop.web").unwrap().short, "web");
        assert_eq!(s.apps_in("shop").unwrap().len(), 1);
        assert!(s.deploy("nope.web", &m, &serde_json::json!({})).is_err());
        assert!(s.delete_project("shop").is_err(), "has apps");
        s.delete_app("shop.web").unwrap();
        s.delete_project("shop").unwrap();
    }

    #[test]
    fn first_user_is_created_once() {
        let s = store();
        let u = s.create_first_user("Me@Example.com", "Me", "hash").unwrap();
        assert_eq!(u.email, "me@example.com");
        assert!(s.create_first_user("b@example.com", "B", "h").is_err());
        assert_eq!(s.role_of(&u.id, "org_default").unwrap(), Some(Role::Owner));
    }

    #[test]
    fn sessions_tokens_and_expiry() {
        let s = store();
        let u = s.create_first_user("a@example.com", "A", "h").unwrap();
        s.create_session("h1", &u.id, 1000, None, None).unwrap();
        assert!(s.session("h1", 1000).unwrap().is_some());
        s.create_session("h2", &u.id, -1, None, None).unwrap();
        assert!(s.session("h2", 1000).unwrap().is_none(), "expired");
        let t = s
            .create_token(
                "ci",
                "oc_abcd",
                "hash1",
                Some(&u.id),
                &TokenScope::Org("org_default".into()),
                Role::Developer,
                None,
            )
            .unwrap();
        assert_eq!(s.token_by_hash("hash1").unwrap().unwrap().id, t.id);
        s.revoke_token(&t.id).unwrap();
        assert!(s.token_by_hash("hash1").unwrap().is_none());
        let t2 = s
            .create_token(
                "old",
                "oc_xxxx",
                "hash2",
                None,
                &TokenScope::Org("org_default".into()),
                Role::Viewer,
                Some(1),
            )
            .unwrap();
        assert!(
            s.token_by_hash("hash2").unwrap().is_none(),
            "expired {t2:?}"
        );
        s.set_user_disabled(&u.id, true).unwrap();
        assert!(
            s.session("h1", 1000).unwrap().is_none(),
            "disabled users lose their sessions"
        );
    }

    #[test]
    fn domains_claims_and_verification() {
        let s = store();
        s.create_org("acme").unwrap();
        s.create_project("acme", "a").unwrap();
        s.create_project("acme", "b").unwrap();
        let da = s.add_domain("a", "shop.example.org").unwrap();
        assert!(da.txt_value.starts_with("orbit-verify="));
        s.add_domain("b", "shop.example.org").unwrap(); // pending claims can coexist
        assert!(s.add_domain("a", "shop.example.org").is_err());
        s.record_domain_check("b", "shop.example.org", true, false, None)
            .unwrap();
        assert_eq!(
            s.verified_domain_project("shop.example.org")
                .unwrap()
                .as_deref(),
            Some("b")
        );
        assert!(
            s.domain("a", "shop.example.org").is_err(),
            "the other pending claim is gone"
        );
        assert!(
            s.add_domain("a", "shop.example.org").is_err(),
            "verified elsewhere"
        );
        assert_eq!(s.pending_domains().unwrap().len(), 0);
    }

    #[test]
    fn ports_are_stable_and_unique() {
        let s = store();
        let a = s.http_port("x").unwrap();
        let b = s.http_port("y").unwrap();
        assert_ne!(a, b);
        assert_eq!(a, s.http_port("x").unwrap());
        assert!(PORT_RANGE.contains(&a));
    }
}
