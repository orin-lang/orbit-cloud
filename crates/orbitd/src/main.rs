//! `orbitd`: the Orbit Cloud node.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::node::Node;

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
    /// Admin bearer token for `/v1`. Without it the API is open (development only).
    #[arg(long, env = "ORBIT_CLOUD_TOKEN", hide_env_values = true)]
    token: Option<String>,
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
    if token.is_none() {
        tracing::warn!(
            "ORBIT_CLOUD_TOKEN is not set: the API is open to anyone who can reach {}",
            args.listen
        );
    }
    let state = AppState {
        node: Arc::clone(&node),
        token,
    };
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!(
        "orbitd {} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        args.listen
    );
    axum::serve(listener, router(state))
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
