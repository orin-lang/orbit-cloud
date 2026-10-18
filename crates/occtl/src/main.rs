//! `occtl`: the command-line client of Orbit Cloud. Talks to `orbitd`'s HTTP API (`ORBIT_CLOUD_URL`, bearer token
//! `ORBIT_CLOUD_TOKEN`).

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
    /// Base URL of orbitd.
    #[arg(
        long,
        env = "ORBIT_CLOUD_URL",
        default_value = "http://127.0.0.1:8080",
        global = true
    )]
    url: String,
    /// Admin token.
    #[arg(long, env = "ORBIT_CLOUD_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
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
}

impl Client {
    fn req(
        &self,
        method: &str,
        path: &str,
        body: Option<(&[u8], Option<&str>)>,
    ) -> Result<(u16, Vec<u8>)> {
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

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let cli = Cli::parse();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let c = Client {
        agent,
        base: cli.url,
        token: cli.token,
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
                std::thread::sleep(Duration::from_millis(300)); // the log writer batches every 200 ms
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
    }
    Ok(())
}
