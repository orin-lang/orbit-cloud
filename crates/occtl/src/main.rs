//! `occtl`: the command-line client of Orbit Cloud. Talks to `orbitd`'s HTTP API (`ORBIT_CLOUD_URL`, bearer token
//! `ORBIT_CLOUD_TOKEN`, or the credentials `occtl login` saved).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(version, about = "Command-line client for Orbit Cloud")]
struct Cli {
    /// Base URL of orbitd (default: the saved login, else http://127.0.0.1:8080).
    #[arg(long, env = "ORBIT_CLOUD_URL", global = true)]
    url: Option<String>,
    /// API token or the break-glass admin token (default: the saved login).
    #[arg(long, env = "ORBIT_CLOUD_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Project to work in (default: the saved login, else `default`).
    #[arg(short, long, env = "ORBIT_CLOUD_PROJECT", global = true)]
    project: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Deploys a module as the next version of APP (and creates the app).
    Deploy {
        /// App name.
        app: String,
        /// The .wasm module.
        module: PathBuf,
        /// A JSON spec file; the flags below override its fields.
        #[arg(long)]
        spec: Option<PathBuf>,
        /// job, service or consumer.
        #[arg(long)]
        kind: Option<String>,
        /// Queue of a consumer.
        #[arg(long)]
        queue: Option<String>,
        /// KV namespace the app may use (repeatable).
        #[arg(long = "kv")]
        kv: Vec<String>,
        /// ADDR:PORT the app may listen on (repeatable).
        #[arg(long)]
        listen: Vec<String>,
        /// HOST:PORT pattern the app may connect to (repeatable).
        #[arg(long)]
        connect: Vec<String>,
        /// K=V environment variable (repeatable).
        #[arg(long)]
        env: Vec<String>,
        /// Program argument (repeatable).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// A persistent directory mounted at /data.
        #[arg(long)]
        files: bool,
        /// Memory limit in MiB.
        #[arg(long)]
        memory_mb: Option<u64>,
        /// Wall-clock limit per run in ms (0 = none).
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Lists apps.
    Apps,
    /// Shows an app with its deployments.
    App { app: String },
    /// Deletes an app with its runs, logs and KV data.
    Delete { app: String },
    /// Makes VERSION of APP active again.
    Rollback { app: String, version: i64 },
    /// Runs a job (or a consumer, by hand) once and prints the run.
    Run {
        app: String,
        /// File with the run's stdin (`-` = this process's stdin).
        #[arg(long)]
        input: Option<String>,
        /// Integer arguments of the entry export, comma-separated.
        #[arg(long)]
        args: Option<String>,
        /// Do not wait for the end.
        #[arg(long)]
        detach: bool,
    },
    /// Lists runs, of one app or all.
    Runs {
        app: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Shows a run's trace.
    Trace { run: String },
    /// Prints log lines; `-f` keeps following.
    Logs {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        contains: Option<String>,
        #[arg(short, long)]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// KV data of an app.
    Kv {
        #[command(subcommand)]
        op: KvOp,
    },
    /// Queues.
    Queue {
        #[command(subcommand)]
        op: QueueOp,
    },
    /// Node status.
    Status,
    /// Signs in with email and password and saves an API token for this machine.
    Login {
        /// Email (asked for when absent).
        #[arg(long)]
        email: Option<String>,
        /// Read the password from stdin instead of asking for it (CI-friendly; also ORBIT_CLOUD_PASSWORD).
        #[arg(long)]
        password_stdin: bool,
        /// Organisation the saved token is scoped to (default: your only organisation).
        #[arg(long)]
        org: Option<String>,
        /// Scope the saved token to a project instead of an organisation (also the default project).
        #[arg(long = "scope-project")]
        scope_project: Option<String>,
        /// Role of the saved token.
        #[arg(long, default_value = "developer")]
        role: String,
        /// Days until the token expires.
        #[arg(long, default_value_t = 90)]
        days: i64,
    },
    /// Forgets the saved login (and revokes its token).
    Logout,
    /// Shows who the saved login or token is.
    Whoami,
    /// Projects.
    Projects {
        #[command(subcommand)]
        op: ProjectOp,
    },
    /// Custom domains of the project.
    Domains {
        #[command(subcommand)]
        op: DomainOp,
    },
    /// HTTP routes of the project.
    Routes {
        #[command(subcommand)]
        op: RouteOp,
    },
    /// API tokens.
    #[command(alias = "tokens")]
    Token {
        #[command(subcommand)]
        op: TokenOp,
    },
    /// The audit log of an organisation.
    Audit {
        /// Organisation (default: your only one).
        #[arg(long)]
        org: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectOp {
    /// Lists the projects you can see.
    #[command(alias = "list")]
    Ls,
    /// Creates a project (its hostname is NAME.<base domain>).
    Create {
        name: String,
        /// Organisation (default: your only one).
        #[arg(long)]
        org: Option<String>,
    },
    /// Deletes a project (`--force` also deletes its apps).
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DomainOp {
    /// Adds a custom domain and prints the DNS records to create.
    Add { host: String },
    /// Lists the domains with their status.
    #[command(alias = "list")]
    Ls,
    /// Checks DNS now.
    Verify { host: String },
    /// Removes a domain.
    Rm { host: String },
}

#[derive(Subcommand, Debug)]
enum RouteOp {
    /// Lists the routes.
    #[command(alias = "list")]
    Ls,
    /// Routes a path prefix to an HTTP service.
    Add {
        /// App (deployed with `"http": true`).
        app: String,
        /// Path prefix, e.g. /api.
        #[arg(long, default_value = "/")]
        path: String,
        /// Only for this host (the project host or one of its domains).
        #[arg(long)]
        host: Option<String>,
    },
    /// Removes a route by id.
    Rm { id: i64 },
}

#[derive(Subcommand, Debug)]
enum TokenOp {
    /// Lists your tokens.
    #[command(alias = "list")]
    Ls,
    /// Creates a token and prints it once.
    Create {
        /// A label.
        #[arg(long)]
        name: String,
        /// Scope: an organisation.
        #[arg(long, conflicts_with = "scope_project")]
        org: Option<String>,
        /// Scope: a project (default: --project).
        #[arg(long = "scope-project")]
        scope_project: Option<String>,
        /// viewer, developer, admin or owner.
        #[arg(long, default_value = "developer")]
        role: String,
        /// Days until it expires (default: never).
        #[arg(long)]
        days: Option<i64>,
    },
    /// Revokes a token.
    Rm { id: String },
}

#[derive(Subcommand, Debug)]
enum KvOp {
    /// Namespaces of APP, or the keys of NS.
    Ls {
        app: String,
        ns: Option<String>,
        #[arg(long, default_value = "")]
        prefix: String,
    },
    /// Prints a value.
    Get {
        app: String,
        ns: String,
        key: String,
    },
    /// Stores a value.
    Put {
        app: String,
        ns: String,
        key: String,
        value: String,
    },
    /// Removes a key.
    Rm {
        app: String,
        ns: String,
        key: String,
    },
}

#[derive(Subcommand, Debug)]
enum QueueOp {
    /// Lists queues with their counts.
    Ls,
    /// Publishes a message (`-` reads it from stdin).
    Send { queue: String, message: String },
    /// Lists dead letters.
    Dead { queue: String },
    /// Makes the dead letters ready again.
    Redrive { queue: String },
}

struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
    project: Option<String>,
}

/// Saved by `occtl login`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    url: String,
    token: String,
    token_id: Option<String>,
    project: Option<String>,
}

fn credentials_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("occtl").join("credentials.json"))
}

fn load_saved() -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(credentials_path()?).ok()?).ok()
}

fn save_credentials(saved: &Saved) -> Result<PathBuf> {
    use std::io::Write as _;
    let path = credentials_path().context("no home directory to save the login in")?;
    std::fs::create_dir_all(path.parent().context("bad path")?)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(serde_json::to_string_pretty(saved)?.as_bytes())?;
    Ok(path)
}

/// Paths that belong to a project.
fn is_scoped(path: &str) -> bool {
    ["/apps", "/runs", "/logs", "/queues", "/node"]
        .iter()
        .any(|p| {
            path == *p || path.starts_with(&format!("{p}/")) || path.starts_with(&format!("{p}?"))
        })
}

impl Client {
    fn req(
        &self,
        method: &str,
        path: &str,
        body: Option<(&[u8], Option<&str>)>,
    ) -> Result<(u16, Vec<u8>)> {
        let scoped;
        let path = match (&self.project, is_scoped(path)) {
            (Some(p), true) => {
                scoped = format!(
                    "{path}{}project={}",
                    if path.contains('?') { '&' } else { '?' },
                    encode(p)
                );
                scoped.as_str()
            }
            _ => path,
        };
        let url = format!("{}/v1{}", self.base.trim_end_matches('/'), path);
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        macro_rules! go {
            ($r:expr) => {{
                let mut r = $r;
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r
            }};
        }
        let resp = match method {
            "GET" => go!(self.agent.get(&url)).call(),
            "DELETE" => go!(self.agent.delete(&url)).call(),
            m => {
                let mut r = go!(if m == "PUT" {
                    self.agent.put(&url)
                } else {
                    self.agent.post(&url)
                });
                if let Some((_, Some(spec))) = body {
                    r = r.header("x-orbit-spec", spec);
                }
                r.send(body.map_or(&[][..], |(b, _)| b))
            }
        }
        .with_context(|| format!("{method} {url}"))?;
        let status = resp.status().as_u16();
        let mut buf = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut buf)?;
        Ok((status, buf))
    }

    fn json(&self, method: &str, path: &str, body: Option<(&[u8], Option<&str>)>) -> Result<Value> {
        let (status, b) = self.req(method, path, body)?;
        let v: Value = if b.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&b).unwrap_or(Value::String(String::from_utf8_lossy(&b).into()))
        };
        if status >= 400 {
            bail!(
                "{status}: {}",
                v.get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| v.to_string(), str::to_owned)
            );
        }
        Ok(v)
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn print_runs(runs: &Value) {
    println!(
        "{:<18} {:<16} {:<8} {:<10} {:>5} {:>8}  STARTED",
        "RUN", "APP", "TRIGGER", "STATUS", "EXIT", "WALL"
    );
    for r in runs.as_array().into_iter().flatten() {
        println!(
            "{:<18} {:<16} {:<8} {:<10} {:>5} {:>8}  {}",
            s(&r["id"]),
            s(&r["app"]),
            s(&r["trigger"]),
            s(&r["status"]),
            s(&r["exit_code"]),
            r["wall_ms"]
                .as_i64()
                .map_or_else(|| "-".into(), |m| format!("{m}ms")),
            s(&r["started_at"])
        );
    }
}

fn print_logs(lines: &Value) -> Option<i64> {
    let mut last = None;
    for l in lines.as_array().into_iter().flatten() {
        println!(
            "{} {:<12} {:<5} {}",
            s(&l["ts"]),
            s(&l["app"]),
            s(&l["level"]),
            s(&l["message"])
        );
        last = l["id"].as_i64();
    }
    last
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let q: Vec<String> = pairs
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", encode(v))))
        .collect();
    if q.is_empty() {
        String::new()
    } else {
        format!("?{}", q.join("&"))
    }
}

fn encode(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<()> {
    let cli = Cli::parse();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let saved = load_saved();
    let from_saved = cli.url.is_none() && cli.token.is_none();
    let c = Client {
        agent,
        base: cli
            .url
            .clone()
            .or_else(|| saved.as_ref().map(|s| s.url.clone()))
            .unwrap_or_else(|| "http://127.0.0.1:8080".into()),
        token: cli.token.clone().or_else(|| {
            // a saved token belongs to the saved server: not to one named with --url
            saved
                .as_ref()
                .filter(|s| cli.url.as_deref().is_none_or(|u| u == s.url))
                .map(|s| s.token.clone())
        }),
        project: cli.project.clone().or_else(|| {
            saved
                .as_ref()
                .filter(|_| from_saved || cli.token.is_none())
                .and_then(|s| s.project.clone())
        }),
    };
    match cli.cmd {
        Cmd::Deploy {
            app,
            module,
            spec,
            kind,
            queue,
            kv,
            listen,
            connect,
            env,
            args,
            files,
            memory_mb,
            timeout_ms,
        } => {
            let bytes = std::fs::read(&module)
                .with_context(|| format!("cannot read {}", module.display()))?;
            let mut sp: Value = match spec {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)
                    .with_context(|| format!("{}", p.display()))?,
                None => json!({}),
            };
            let o = sp
                .as_object_mut()
                .context("the spec must be a JSON object")?;
            if let Some(k) = kind {
                o.insert("kind".into(), json!(k));
            }
            if let Some(q) = queue {
                o.insert("queue".into(), json!(q));
            }
            if !kv.is_empty() {
                o.insert("kv_namespaces".into(), json!(kv));
            }
            if !listen.is_empty() {
                o.insert("listen".into(), json!(listen));
            }
            if !connect.is_empty() {
                o.insert("connect".into(), json!(connect));
            }
            if !args.is_empty() {
                o.insert("args".into(), json!(args));
            }
            if files {
                o.insert("files".into(), json!(true));
            }
            if !env.is_empty() {
                let mut m = BTreeMap::new();
                for kv in env {
                    let (k, v) = kv
                        .split_once('=')
                        .with_context(|| format!("bad --env `{kv}`: expected K=V"))?;
                    m.insert(k.to_owned(), v.to_owned());
                }
                o.insert("env".into(), json!(m));
            }
            if memory_mb.is_some() || timeout_ms.is_some() {
                let l = o.entry("limits").or_insert_with(|| json!({}));
                if let Some(m) = memory_mb {
                    l["memory_mb"] = json!(m);
                }
                if let Some(t) = timeout_ms {
                    l["timeout_ms"] = json!(t);
                }
            }
            let d = c.json(
                "POST",
                &format!("/apps/{app}/deployments"),
                Some((&bytes, Some(&sp.to_string()))),
            )?;
            println!(
                "deployed {app} version {} (deployment {}, module {})",
                s(&d["version"]),
                s(&d["id"]),
                s(&d["module"])
            );
        }
        Cmd::Apps => {
            let apps = c.json("GET", "/apps", None)?;
            println!("{:<24} {:<9} {:>7}  WORKER", "APP", "KIND", "VERSION");
            for a in apps.as_array().into_iter().flatten() {
                let w = if a["kind"] == "job" {
                    ""
                } else if a["worker_running"] == true {
                    "running"
                } else {
                    "stopped"
                };
                println!(
                    "{:<24} {:<9} {:>7}  {w}",
                    s(&a["name"]),
                    s(&a["kind"]),
                    s(&a["version"])
                );
            }
        }
        Cmd::App { app } => println!(
            "{}",
            serde_json::to_string_pretty(&c.json("GET", &format!("/apps/{app}"), None)?)?
        ),
        Cmd::Delete { app } => {
            c.json("DELETE", &format!("/apps/{app}"), None)?;
            println!("deleted {app}");
        }
        Cmd::Rollback { app, version } => {
            let a = c.json("GET", &format!("/apps/{app}"), None)?;
            let id = a["deployments"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|d| d["version"] == version)
                .and_then(|d| d["id"].as_i64())
                .with_context(|| format!("{app} has no version {version}"))?;
            c.json(
                "POST",
                &format!("/apps/{app}/deployments/{id}/activate"),
                Some((&[], None)),
            )?;
            println!("{app} now runs version {version}");
        }
        Cmd::Run {
            app,
            input,
            args,
            detach,
        } => {
            let body = match input.as_deref() {
                Some("-") => {
                    let mut b = Vec::new();
                    std::io::stdin().read_to_end(&mut b)?;
                    b
                }
                Some(p) => std::fs::read(p)?,
                None => Vec::new(),
            };
            let q = query(&[("args", args), ("detach", detach.then(|| "true".into()))]);
            let r = c.json("POST", &format!("/apps/{app}/runs{q}"), Some((&body, None)))?;
            println!(
                "run {} {} exit {} ({} ms)",
                s(&r["id"]),
                s(&r["status"]),
                s(&r["exit_code"]),
                s(&r["wall_ms"])
            );
            if let Some(e) = r["error"].as_str() {
                println!("error: {e}");
            }
            if !detach {
                print_logs(&c.json("GET", &format!("/runs/{}/logs", s(&r["id"])), None)?);
                if r["status"] != "succeeded" {
                    std::process::exit(1);
                }
            }
        }
        Cmd::Runs { app, limit } => {
            let path = app.map_or_else(
                || format!("/runs?limit={limit}"),
                |a| format!("/apps/{a}/runs?limit={limit}"),
            );
            print_runs(&c.json("GET", &path, None)?);
        }
        Cmd::Trace { run } => {
            let t = c.json("GET", &format!("/runs/{run}/trace"), None)?;
            let spans = t["spans"].as_array().cloned().unwrap_or_default();
            let t0 = spans
                .iter()
                .filter_map(|s| s["start_us"].as_i64())
                .min()
                .unwrap_or(0);
            println!("trace {}", s(&t["trace_id"]));
            for sp in &spans {
                let (a, b) = (
                    sp["start_us"].as_i64().unwrap_or(0),
                    sp["end_us"].as_i64().unwrap_or(0),
                );
                let ind = if sp["parent_id"].is_null() { "" } else { "  " };
                #[allow(clippy::cast_precision_loss)]
                let (off, dur) = ((a - t0) as f64 / 1000.0, (b - a) as f64 / 1000.0);
                println!(
                    "{ind}{:<14} +{off:>9.3} ms  {dur:>9.3} ms  {}",
                    s(&sp["name"]),
                    sp["attrs"]
                );
            }
        }
        Cmd::Logs {
            app,
            run,
            level,
            contains,
            follow,
            limit,
        } => {
            let base = [
                ("app", app),
                ("run", run),
                ("level", level),
                ("contains", contains),
            ];
            let mut q: Vec<(&str, Option<String>)> = base.to_vec();
            q.push(("limit", Some(limit.to_string())));
            let mut last = print_logs(&c.json("GET", &format!("/logs{}", query(&q)), None)?);
            if follow {
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let mut q: Vec<(&str, Option<String>)> = base.to_vec();
                    q.push(("after", Some(last.unwrap_or(0).to_string())));
                    q.push(("limit", Some("1000".into())));
                    if let Some(id) =
                        print_logs(&c.json("GET", &format!("/logs{}", query(&q)), None)?)
                    {
                        last = Some(id);
                    }
                }
            }
        }
        Cmd::Kv { op } => match op {
            KvOp::Ls { app, ns: None, .. } => {
                for n in c
                    .json("GET", &format!("/apps/{app}/kv"), None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!("{}\t{}", s(&n["namespace"]), s(&n["entries"]));
                }
            }
            KvOp::Ls {
                app,
                ns: Some(ns),
                prefix,
            } => {
                let q = query(&[("prefix", Some(prefix))]);
                for e in c
                    .json("GET", &format!("/apps/{app}/kv/{}{q}", encode(&ns)), None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!("{}\t{}", s(&e["key"]), s(&e["value"]));
                }
            }
            KvOp::Get { app, ns, key } => {
                let (st, b) = c.req(
                    "GET",
                    &format!("/apps/{app}/kv/{}/{}", encode(&ns), encode(&key)),
                    None,
                )?;
                if st >= 400 {
                    bail!("{st}: {}", String::from_utf8_lossy(&b));
                }
                println!("{}", String::from_utf8_lossy(&b));
            }
            KvOp::Put {
                app,
                ns,
                key,
                value,
            } => {
                c.json(
                    "PUT",
                    &format!("/apps/{app}/kv/{}/{}", encode(&ns), encode(&key)),
                    Some((value.as_bytes(), None)),
                )?;
            }
            KvOp::Rm { app, ns, key } => {
                c.json(
                    "DELETE",
                    &format!("/apps/{app}/kv/{}/{}", encode(&ns), encode(&key)),
                    None,
                )?;
            }
        },
        Cmd::Queue { op } => match op {
            QueueOp::Ls => {
                println!(
                    "{:<24} {:>7} {:>7} {:>7} {:>7}",
                    "QUEUE", "READY", "LEASED", "DEAD", "DONE"
                );
                for q in c
                    .json("GET", "/queues", None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{:<24} {:>7} {:>7} {:>7} {:>7}",
                        s(&q["queue"]),
                        s(&q["ready"]),
                        s(&q["leased"]),
                        s(&q["dead"]),
                        s(&q["done"])
                    );
                }
            }
            QueueOp::Send { queue, message } => {
                let body = if message == "-" {
                    let mut b = Vec::new();
                    std::io::stdin().read_to_end(&mut b)?;
                    b
                } else {
                    message.into_bytes()
                };
                let r = c.json(
                    "POST",
                    &format!("/queues/{queue}/messages"),
                    Some((&body, None)),
                )?;
                println!("message {} on {queue}", s(&r["id"]));
            }
            QueueOp::Dead { queue } => {
                for m in c
                    .json("GET", &format!("/queues/{queue}/dead"), None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{}\tattempts {}\t{}\t{}",
                        s(&m["id"]),
                        s(&m["attempts"]),
                        s(&m["last_error"]),
                        s(&m["body"])
                    );
                }
            }
            QueueOp::Redrive { queue } => {
                let r = c.json(
                    "POST",
                    &format!("/queues/{queue}/redrive"),
                    Some((&[], None)),
                )?;
                println!("{} message(s) ready again", s(&r["redriven"]));
            }
        },
        Cmd::Status => println!(
            "{}",
            serde_json::to_string_pretty(&c.json("GET", "/node", None)?)?
        ),
        Cmd::Login {
            email,
            password_stdin,
            org,
            scope_project,
            role,
            days,
        } => login(&c, email, password_stdin, org, scope_project, &role, days)?,
        Cmd::Logout => {
            if let (Some(sv), Some(path)) = (load_saved(), credentials_path()) {
                if let Some(id) = &sv.token_id {
                    let _ = c.json("DELETE", &format!("/tokens/{id}"), None);
                }
                std::fs::remove_file(path)?;
                println!("signed out of {}", sv.url);
            } else {
                println!("no saved login");
            }
        }
        Cmd::Whoami => println!(
            "{}",
            serde_json::to_string_pretty(&c.json("GET", "/me", None)?)?
        ),
        Cmd::Projects { op } => match op {
            ProjectOp::Ls => {
                println!("{:<24} {:<20} HOST", "PROJECT", "ORG");
                for p in c
                    .json("GET", "/projects", None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{:<24} {:<20} {}",
                        s(&p["name"]),
                        s(&p["org"]),
                        s(&p["host"])
                    );
                }
            }
            ProjectOp::Create { name, org } => {
                let body = json!({ "name": name, "org": org });
                let p = c.json(
                    "POST",
                    "/projects",
                    Some((body.to_string().as_bytes(), None)),
                )?;
                println!("created project {} ({})", s(&p["name"]), s(&p["host"]));
            }
            ProjectOp::Rm { name, force } => {
                let q = if force { "?force=true" } else { "" };
                c.json("DELETE", &format!("/projects/{}{q}", encode(&name)), None)?;
                println!("deleted project {name}");
            }
        },
        Cmd::Domains { op } => {
            let p = encode(&need_project(&c)?);
            match op {
                DomainOp::Add { host } => {
                    let body = json!({ "host": host });
                    let d = c.json(
                        "POST",
                        &format!("/projects/{p}/domains"),
                        Some((body.to_string().as_bytes(), None)),
                    )?;
                    print_domain(&d);
                }
                DomainOp::Ls => {
                    println!("{:<36} {:<10} {:<6} CHECKED", "HOST", "STATUS", "DNS");
                    for d in c
                        .json("GET", &format!("/projects/{p}/domains"), None)?
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        println!(
                            "{:<36} {:<10} {:<6} {}",
                            s(&d["host"]),
                            s(&d["status"]),
                            if d["cname_ok"] == true { "ok" } else { "-" },
                            s(&d["last_error"])
                        );
                    }
                }
                DomainOp::Verify { host } => {
                    let d = c.json(
                        "POST",
                        &format!("/projects/{p}/domains/{}/verify", encode(&host)),
                        Some((&[], None)),
                    )?;
                    print_domain(&d);
                }
                DomainOp::Rm { host } => {
                    c.json(
                        "DELETE",
                        &format!("/projects/{p}/domains/{}", encode(&host)),
                        None,
                    )?;
                    println!("removed {host}");
                }
            }
        }
        Cmd::Routes { op } => {
            let p = encode(&need_project(&c)?);
            match op {
                RouteOp::Ls => {
                    println!("{:<5} {:<28} {:<14} APP", "ID", "HOST", "PATH");
                    for r in c
                        .json("GET", &format!("/projects/{p}/routes"), None)?
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        let host = if r["host"].is_null() {
                            "(all hosts)".to_owned()
                        } else {
                            s(&r["host"])
                        };
                        println!(
                            "{:<5} {:<28} {:<14} {}",
                            s(&r["id"]),
                            host,
                            s(&r["path_prefix"]),
                            s(&r["app"])
                        );
                    }
                }
                RouteOp::Add { app, path, host } => {
                    let body = json!({ "app": app, "path_prefix": path, "host": host });
                    let r = c.json(
                        "POST",
                        &format!("/projects/{p}/routes"),
                        Some((body.to_string().as_bytes(), None)),
                    )?;
                    println!("route {} added", s(&r["id"]));
                }
                RouteOp::Rm { id } => {
                    c.json("DELETE", &format!("/projects/{p}/routes/{id}"), None)?;
                    println!("removed route {id}");
                }
            }
        }
        Cmd::Token { op } => match op {
            TokenOp::Ls => {
                println!(
                    "{:<14} {:<12} {:<20} {:<10} {:<28} EXPIRES",
                    "ID", "PREFIX", "NAME", "ROLE", "SCOPE"
                );
                for t in c
                    .json("GET", "/tokens", None)?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{:<14} {:<12} {:<20} {:<10} {:<28} {}",
                        s(&t["id"]),
                        s(&t["prefix"]),
                        s(&t["name"]),
                        s(&t["role"]),
                        t["scope"].to_string(),
                        if t["revoked_at"].is_null() {
                            s(&t["expires_at"])
                        } else {
                            "revoked".into()
                        }
                    );
                }
            }
            TokenOp::Create {
                name,
                org,
                scope_project,
                role,
                days,
            } => {
                let scope = match (org, scope_project.or_else(|| c.project.clone())) {
                    (Some(o), _) => json!({ "org": o }),
                    (None, Some(p)) => json!({ "project": p }),
                    (None, None) => bail!("name the scope: --org ORG or --scope-project PROJECT"),
                };
                let body =
                    json!({ "name": name, "scope": scope, "role": role, "expires_in_days": days });
                let t = c.json("POST", "/tokens", Some((body.to_string().as_bytes(), None)))?;
                println!("{}", s(&t["token"]));
                eprintln!("token {} created; it is shown only once", s(&t["id"]));
            }
            TokenOp::Rm { id } => {
                c.json("DELETE", &format!("/tokens/{}", encode(&id)), None)?;
                println!("revoked {id}");
            }
        },
        Cmd::Audit { org, limit } => {
            let org = match org {
                Some(o) => o,
                None => only_org(&c)?,
            };
            for e in c
                .json(
                    "GET",
                    &format!("/orgs/{}/audit?limit={limit}", encode(&org)),
                    None,
                )?
                .as_array()
                .into_iter()
                .flatten()
            {
                println!(
                    "{} {:<10} {:<28} {:<16} {}",
                    s(&e["ts"]),
                    s(&e["actor_kind"]),
                    s(&e["actor_label"]),
                    s(&e["action"]),
                    s(&e["target"])
                );
            }
        }
    }
    Ok(())
}

fn need_project(c: &Client) -> Result<String> {
    c.project
        .clone()
        .context("name the project: --project NAME (or ORBIT_CLOUD_PROJECT, or `occtl login --scope-project`)")
}

/// The caller's only organisation.
fn only_org(c: &Client) -> Result<String> {
    let orgs = c.json("GET", "/orgs", None)?;
    match orgs.as_array().map(Vec::as_slice) {
        Some([o]) => Ok(s(&o["name"])),
        _ => bail!("you are in several organisations: pass --org NAME"),
    }
}

fn print_domain(d: &Value) {
    println!("{} is {}", s(&d["host"]), s(&d["status"]));
    if d["status"] != "verified" {
        println!(
            "create these DNS records, then run `occtl domains verify {}`:",
            s(&d["host"])
        );
        println!(
            "  TXT    {}  \"{}\"",
            s(&d["txt"]["name"]),
            s(&d["txt"]["value"])
        );
        println!(
            "  CNAME  {}  {}   (or A/AAAA records to the same addresses)",
            s(&d["cname"]["name"]),
            s(&d["cname"]["value"])
        );
        if !d["last_error"].is_null() {
            println!("last check: {}", s(&d["last_error"]));
        }
    } else if d["cname_ok"] != true {
        println!(
            "note: {} does not point at {} yet",
            s(&d["host"]),
            s(&d["cname"]["value"])
        );
    }
}

/// Signs in, creates an API token and saves it.
fn login(
    c: &Client,
    email: Option<String>,
    password_stdin: bool,
    org: Option<String>,
    scope_project: Option<String>,
    role: &str,
    days: i64,
) -> Result<()> {
    let email = match email {
        Some(e) => e,
        None => {
            eprint!("Email: ");
            let mut l = String::new();
            std::io::stdin().read_line(&mut l)?;
            l.trim().to_owned()
        }
    };
    let password = if password_stdin {
        let mut l = String::new();
        std::io::stdin().read_line(&mut l)?;
        l.trim_end_matches(['\r', '\n']).to_owned()
    } else if let Ok(p) = std::env::var("ORBIT_CLOUD_PASSWORD") {
        p
    } else {
        rpassword::prompt_password("Password: ")?
    };
    let url = format!("{}/v1/auth/login", c.base.trim_end_matches('/'));
    let resp = c
        .agent
        .post(&url)
        .send(
            json!({ "email": email, "password": password })
                .to_string()
                .as_bytes(),
        )
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::to_owned);
    let mut buf = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    let v: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    if status >= 400 {
        bail!(
            "{status}: {}",
            v["error"].as_str().unwrap_or("login failed")
        );
    }
    let cookie = cookie.context("the server sent no session cookie")?;
    let csrf = v["csrf_token"]
        .as_str()
        .context("no csrf token in the login answer")?
        .to_owned();
    let scope = match (scope_project.clone(), org) {
        (Some(p), _) => json!({ "project": p }),
        (None, Some(o)) => json!({ "org": o }),
        (None, None) => {
            let orgs = v["memberships"].as_array().cloned().unwrap_or_default();
            match orgs.as_slice() {
                [m] => json!({ "org": m["org"] }),
                _ => bail!(
                    "you are in {} organisations: pass --org NAME or --scope-project NAME",
                    orgs.len()
                ),
            }
        }
    };
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "this machine".into());
    let turl = format!("{}/v1/tokens", c.base.trim_end_matches('/'));
    let resp = c
        .agent
        .post(&turl)
        .header("Content-Type", "application/json")
        .header("Cookie", &cookie)
        .header("X-CSRF-Token", &csrf)
        .send(
            json!({ "name": format!("occtl on {host}"), "scope": scope, "role": role, "expires_in_days": days })
                .to_string()
                .as_bytes(),
        )
        .with_context(|| format!("POST {turl}"))?;
    let status = resp.status().as_u16();
    let mut buf = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    let t: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    if status >= 400 {
        bail!(
            "{status}: {}",
            t["error"].as_str().unwrap_or("cannot create a token")
        );
    }
    // end the browser-style session: the token is all this machine keeps
    let _ = c
        .agent
        .post(&format!("{}/v1/auth/logout", c.base.trim_end_matches('/')))
        .header("Cookie", &cookie)
        .header("X-CSRF-Token", &csrf)
        .send(&[][..]);
    let path = save_credentials(&Saved {
        url: c.base.clone(),
        token: s(&t["token"]),
        token_id: t["id"].as_str().map(str::to_owned),
        project: scope_project.or_else(|| c.project.clone()),
    })?;
    println!(
        "signed in as {} on {}; token {} ({role}, {days} days) saved in {}",
        s(&v["user"]["email"]),
        c.base,
        s(&t["prefix"]),
        path.display()
    );
    Ok(())
}
