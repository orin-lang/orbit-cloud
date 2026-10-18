//! `orbitd`: the Orbit Cloud node.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, ValueEnum};
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::gateway::dns::SystemDns;
use orbitd::gateway::forwarded::TrustedProxies;
use orbitd::gateway::proxy::ProxyConfig;
use orbitd::gateway::tls::{LETS_ENCRYPT, LETS_ENCRYPT_STAGING};
use orbitd::gateway::{Gateway, GatewayConfig, TlsSettings, build_tls, domains};
use orbitd::node::Node;
use orbitd::platform::Platform;

/// HTTPS certificate handling.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum TlsMode {
    /// Serve HTTPS with the operator certificate and/or ACME certificates (needs `--https`).
    Auto,
    /// No TLS here: run behind a reverse proxy that terminates it (see `--trusted-proxy`).
    Off,
}

/// Runs WebAssembly apps on Orbit with logs, traces, metrics, KV and queues.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address of the HTTP API and console.
    #[arg(long, env = "ORBIT_CLOUD_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Directory of the database and the apps' files.
    #[arg(long, env = "ORBIT_CLOUD_DATA", default_value = "/var/lib/orbit-cloud")]
    data_dir: PathBuf,
    /// Break-glass admin bearer token for `/v1`. Without it, and before the first user exists, the API is open
    /// (development only).
    #[arg(long, env = "ORBIT_CLOUD_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Base domain of the platform: projects are served at `<project>.<base>`, the console at `console.<base>`.
    #[arg(long, env = "ORBIT_CLOUD_BASE_DOMAIN")]
    base_domain: Option<String>,
    /// Public HTTP listener of the gateway, e.g. `0.0.0.0:80` (needs `--base-domain`).
    #[arg(long, env = "ORBIT_CLOUD_HTTP")]
    http: Option<SocketAddr>,
    /// Public HTTPS listener of the gateway, e.g. `0.0.0.0:443` (needs `--base-domain`).
    #[arg(long, env = "ORBIT_CLOUD_HTTPS")]
    https: Option<SocketAddr>,
    /// `auto`: HTTPS with certificates for known hosts; `off`: no TLS here (behind a reverse proxy).
    #[arg(long, env = "ORBIT_CLOUD_TLS", value_enum, default_value = "auto")]
    tls: TlsMode,
    /// Operator-provided certificate chain (PEM), typically a wildcard for `*.<base>`.
    #[arg(long, env = "ORBIT_CLOUD_TLS_CERT")]
    tls_cert: Option<PathBuf>,
    /// Private key (PEM) of `--tls-cert`.
    #[arg(long, env = "ORBIT_CLOUD_TLS_KEY")]
    tls_key: Option<PathBuf>,
    /// Contact email for ACME (Let's Encrypt); turns on automatic certificates for known hosts.
    #[arg(long, env = "ORBIT_CLOUD_ACME_EMAIL")]
    acme_email: Option<String>,
    /// Use the Let's Encrypt staging CA.
    #[arg(long, env = "ORBIT_CLOUD_ACME_STAGING")]
    acme_staging: bool,
    /// ACME directory URL (default: Let's Encrypt).
    #[arg(long, env = "ORBIT_CLOUD_ACME_DIRECTORY")]
    acme_directory: Option<String>,
    /// Extra root certificate (PEM) to trust for the ACME directory (for a private CA such as Pebble).
    #[arg(long, env = "ORBIT_CLOUD_ACME_CA_FILE")]
    acme_ca_file: Option<PathBuf>,
    /// Peers (address or CIDR) whose `X-Forwarded-For`/`-Proto` are believed. Repeat or comma-separate.
    #[arg(
        long = "trusted-proxy",
        env = "ORBIT_CLOUD_TRUSTED_PROXIES",
        value_delimiter = ','
    )]
    trusted_proxies: Vec<String>,
    /// Largest request body the gateway forwards to an app, MiB.
    #[arg(long, env = "ORBIT_CLOUD_MAX_BODY_MB", default_value_t = 100)]
    max_body_mb: u64,
    /// How long an app may take to answer a proxied request, seconds.
    #[arg(long, env = "ORBIT_CLOUD_UPSTREAM_TIMEOUT_SECS", default_value_t = 60)]
    upstream_timeout_secs: u64,
    /// Meter fuel so runs report it and `limits.fuel` works (costs some speed).
    #[arg(long, env = "ORBIT_CLOUD_FUEL_METERING")]
    fuel_metering: bool,
    /// Probe `/healthz` on the listen address and exit 0 if the node answers (for container health checks).
    #[arg(long)]
    healthcheck: bool,
}

/// GET `/healthz` over plain TCP; true on a 200. An unspecified listen address is probed on loopback.
fn healthcheck(listen: SocketAddr) -> bool {
    use std::io::{Read as _, Write as _};
    let mut addr = listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let t = std::time::Duration::from_secs(3);
    let Ok(mut c) = std::net::TcpStream::connect_timeout(&addr, t) else {
        return false;
    };
    let _ = c.set_read_timeout(Some(t));
    let req = format!("GET /healthz HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    let mut head = [0u8; 12];
    c.write_all(req.as_bytes()).is_ok()
        && c.read_exact(&mut head).is_ok()
        && (head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    if args.healthcheck {
        std::process::exit(i32::from(!healthcheck(args.listen)));
    }
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("cannot create {}", args.data_dir.display()))?;
    let store = Arc::new(
        oc_store::Store::open(&args.data_dir.join("orbit-cloud.db"))
            .context("open the database")?,
    );
    let recovered = store.recover()?;
    if recovered > 0 {
        tracing::warn!("{recovered} run(s) were active when orbitd last stopped; marked failed");
    }
    let executor = Arc::new(Executor::new(
        Arc::clone(&store),
        &args.data_dir,
        args.fuel_metering,
    )?);
    let node = Node::new(executor);
    node.start_all().await?;
    let token = args.token.filter(|t| !t.is_empty()).map(Arc::from);
    let base_domain = args
        .base_domain
        .as_deref()
        .map(|b| b.trim().trim_end_matches('.').to_lowercase())
        .filter(|b| !b.is_empty());
    // A base domain alone means "run the gateway": HTTP on 80 and, unless TLS is off, HTTPS on 443.
    let (http, https) = if base_domain.is_some() && args.http.is_none() && args.https.is_none() {
        (
            Some(SocketAddr::from(([0, 0, 0, 0], 80))),
            (args.tls == TlsMode::Auto).then(|| SocketAddr::from(([0, 0, 0, 0], 443))),
        )
    } else {
        (args.http, args.https)
    };
    let acme_email = args.acme_email.clone().filter(|e| !e.trim().is_empty());
    let gateway_wanted = http.is_some() || https.is_some();
    if gateway_wanted && base_domain.is_none() {
        anyhow::bail!("--http / --https need --base-domain");
    }
    if https.is_some() && args.tls == TlsMode::Off {
        anyhow::bail!(
            "--https and --tls off contradict each other; drop --https when TLS ends at a proxy"
        );
    }
    let platform = Arc::new(Platform::new(
        base_domain.clone(),
        Arc::new(SystemDns::default()),
    ));
    let (verified_tx, verified_rx) = tokio::sync::mpsc::unbounded_channel();
    *platform
        .verified_tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(verified_tx.clone());
    let state = AppState {
        node: Arc::clone(&node),
        token: token.clone(),
        platform: Arc::clone(&platform),
    };
    if let Some(setup) = platform.init_setup_token(&store) {
        let tls = https.is_some() && args.tls == TlsMode::Auto;
        let (scheme, port, default) = if tls {
            ("https", https.map(|a| a.port()), 443)
        } else {
            ("http", http.map(|a| a.port()), 80)
        };
        let port = port
            .filter(|p| *p != default)
            .map_or_else(String::new, |p| format!(":{p}"));
        let origin = match &base_domain {
            Some(b) if gateway_wanted => format!("{scheme}://console.{b}{port}"),
            _ => format!("http://{}", args.listen),
        };
        tracing::info!("no users yet. Create the first owner at {origin}/setup?token={setup}");
    } else if token.is_none() && store.user_count()? == 0 {
        tracing::warn!(
            "ORBIT_CLOUD_TOKEN is not set: the API is open to anyone who can reach {}",
            args.listen
        );
    }
    if token.is_none() && store.user_count()? > 0 {
        tracing::info!(
            "ORBIT_CLOUD_TOKEN is not set: only users and API tokens can use the API (no break-glass token)"
        );
    }

    // background: domain verification, expired sessions
    tokio::spawn(domains::verifier_loop(
        Arc::clone(&store),
        Arc::clone(&platform.dns),
        base_domain.clone(),
        verified_tx,
    ));
    {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                t.tick().await;
                let _ = store.purge_sessions();
            }
        });
    }

    // the gateway
    let mut verified_rx = Some(verified_rx);
    if let (true, Some(base)) = (gateway_wanted, base_domain.clone()) {
        let tls_on = https.is_some() && args.tls == TlsMode::Auto;
        let (tls, challenges) = if tls_on {
            let (m, c) = build_tls(
                &store,
                &TlsSettings {
                    cert: args.tls_cert.clone(),
                    key: args.tls_key.clone(),
                    acme_email: acme_email.clone(),
                    acme_directory: args.acme_directory.clone().unwrap_or_else(|| {
                        if args.acme_staging {
                            LETS_ENCRYPT_STAGING
                        } else {
                            LETS_ENCRYPT
                        }
                        .to_owned()
                    }),
                    acme_ca_file: args.acme_ca_file.clone(),
                },
            )
            .map_err(anyhow::Error::msg)?;
            if args.tls_cert.is_none() && acme_email.is_none() {
                tracing::warn!(
                    "--https without --tls-cert or --acme-email: only certificates already in the database can be served"
                );
            }
            (Some(m), c)
        } else {
            (None, Arc::default())
        };
        let cfg = GatewayConfig {
            base_domain: base.clone(),
            https_port: https.map(|a| a.port()),
            redirect_to_https: tls_on,
            trusted: TrustedProxies::parse(&args.trusted_proxies).map_err(anyhow::Error::msg)?,
            proxy: ProxyConfig {
                max_body: args.max_body_mb.saturating_mul(1024 * 1024),
                upstream_timeout: std::time::Duration::from_secs(args.upstream_timeout_secs),
            },
        };
        let gw = Gateway::new(
            Arc::clone(&store),
            router(state.clone()),
            cfg,
            tls,
            challenges,
        );
        if let Some(rx) = verified_rx.take() {
            gw.spawn_background(rx);
        }
        if let Some(addr) = http {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            tracing::info!("gateway: http://{addr} (base domain {base})");
            tokio::spawn(Arc::clone(&gw).serve_http(l));
        }
        if let Some(addr) = https {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            tracing::info!("gateway: https://{addr}");
            tokio::spawn(Arc::clone(&gw).serve_https(l));
        }
    }
    drop(verified_rx);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!(
        "orbitd {} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        args.listen
    );
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    tracing::info!("stopping workers");
    node.stop_all().await;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::healthcheck;

    fn serve_once(reply: &'static [u8]) -> std::net::SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 256];
                let _ = s.read(&mut buf);
                let _ = s.write_all(reply);
            }
        });
        addr
    }

    #[test]
    fn healthcheck_needs_a_200() {
        assert!(healthcheck(serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
        )));
        assert!(!healthcheck(serve_once(
            b"HTTP/1.1 503 Service Unavailable\r\n\r\n"
        )));
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        assert!(!healthcheck(closed));
    }
}
