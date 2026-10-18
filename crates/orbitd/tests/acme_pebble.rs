//! ACME against a real CA test server (Pebble). Opt-in: needs the `pebble` and `pebble-challtestsrv` binaries
//! (`go install github.com/letsencrypt/pebble/v2/cmd/pebble@latest` and `.../cmd/pebble-challtestsrv@latest`):
//!
//! ```sh
//! PEBBLE_BIN=$(which pebble) PEBBLE_CHALLTESTSRV_BIN=$(which pebble-challtestsrv) \
//!   cargo test -p orbitd --test acme_pebble -- --nocapture
//! ```
//!
//! Without the variables the test returns at once (CI does not have Pebble). It uses fixed local ports 14000, 15000,
//! 8053, 8055 and 5002 (Pebble's HTTP-01 validation port), so run it alone.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::gateway::tls::{AcmeIssuer, CertIssuer, Challenges, TlsManager};
use orbitd::gateway::{Gateway, GatewayConfig};
use orbitd::node::Node;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nothing listens on {port}");
}

#[tokio::test]
async fn orders_a_certificate_on_demand_for_a_known_host_only() {
    let (Some(pebble), Some(chall)) = (
        std::env::var_os("PEBBLE_BIN"),
        std::env::var_os("PEBBLE_CHALLTESTSRV_BIN"),
    ) else {
        eprintln!("skipped: set PEBBLE_BIN and PEBBLE_CHALLTESTSRV_BIN to run the ACME test");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("orbitd=debug,instant_acme=debug")
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    // Pebble's own API certificate
    let api =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let cert_path = dir.path().join("pebble-cert.pem");
    let key_path = dir.path().join("pebble-key.pem");
    std::fs::write(&cert_path, api.cert.pem()).unwrap();
    std::fs::write(&key_path, api.signing_key.serialize_pem()).unwrap();
    let cfg = serde_json::json!({"pebble": {
        "listenAddress": "127.0.0.1:14000", "managementListenAddress": "127.0.0.1:15000",
        "certificate": cert_path, "privateKey": key_path, "httpPort": 5002, "tlsPort": 5001,
        "ocspResponderURL": "", "externalAccountBindingRequired": false }});
    std::fs::write(dir.path().join("pebble.json"), cfg.to_string()).unwrap();
    let _dns = Proc(
        Command::new(chall)
            .args([
                "-http01",
                "",
                "-https01",
                "",
                "-tlsalpn01",
                "",
                "-doh",
                "",
                "-defaultIPv6",
                "",
                "-dnsserver",
                "127.0.0.1:8053",
                "-management",
                "127.0.0.1:8055",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let _ca = Proc(
        Command::new(pebble)
            .args([
                "-config",
                dir.path().join("pebble.json").to_str().unwrap(),
                "-dnsserver",
                "127.0.0.1:8053",
            ])
            .env("PEBBLE_VA_NOSLEEP", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(14000).await;
    wait_for_port(8053).await;

    // the gateway: HTTP on 5002 (where Pebble validates), HTTPS on any port
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let challenges = Arc::new(Challenges::default());
    let issuer: Arc<dyn CertIssuer> = Arc::new(Arc::new(AcmeIssuer::new(
        Arc::clone(&store),
        "ops@example.test".into(),
        "https://127.0.0.1:14000/dir".into(),
        Some(cert_path.clone()),
        Arc::clone(&challenges),
    )));
    let tls = Arc::new(TlsManager::new(Arc::clone(&store), None, Some(issuer)));
    let node = Node::new(Arc::new(
        Executor::new(Arc::clone(&store), dir.path(), false).unwrap(),
    ));
    let gw = Gateway::new(
        Arc::clone(&store),
        router(AppState::new(node, Some(Arc::from("admin")))),
        GatewayConfig::plain("cloud.test"),
        Some(tls),
        challenges,
    );
    tokio::spawn(Arc::clone(&gw).serve_http(TcpListener::bind("127.0.0.1:5002").await.unwrap()));
    let https = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = https.local_addr().unwrap();
    tokio::spawn(Arc::clone(&gw).serve_https(https));

    // an app behind the project
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_: Request<hyper::body::Incoming>| async {
                    Ok::<_, std::convert::Infallible>(Response::new(Body::from(
                        "hello from the app",
                    )))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    store.create_project("default", "shop").unwrap();
    let m = store.put_module(b"\0asm\x01\0\0\0").unwrap();
    store
        .deploy(
            "shop.web",
            &m,
            &serde_json::json!({"kind": "service", "http": true}),
        )
        .unwrap();
    store.assign_http_port("shop.web", port).unwrap();
    store.add_route("shop", "", "/", "shop.web").unwrap();

    // Pebble's root, from its management interface
    let mut roots = rustls::RootCertStore::empty();
    {
        let mut pc = rustls::RootCertStore::empty();
        pc.add(
            rustls_pemfile::certs(&mut api.cert.pem().as_bytes())
                .next()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let c = tokio_rustls::TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(pc)
                .with_no_client_auth(),
        ));
        let tcp = TcpStream::connect("127.0.0.1:15000").await.unwrap();
        let mut s = c
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                tcp,
            )
            .await
            .unwrap();
        s.write_all(b"GET /roots/0 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        let text = String::from_utf8_lossy(&out).into_owned();
        let pem = text.split_once("\r\n\r\n").unwrap().1;
        for c in rustls_pemfile::certs(&mut pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
    }
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));

    // unknown host: refused without ever asking the CA
    let tcp = TcpStream::connect(addr).await.unwrap();
    assert!(
        connector
            .connect(
                rustls::pki_types::ServerName::try_from("evil.cloud.test").unwrap(),
                tcp
            )
            .await
            .is_err()
    );
    assert!(
        store
            .acme_account("https://127.0.0.1:14000/dir")
            .unwrap()
            .is_none(),
        "no ACME traffic for unknown names"
    );

    // known host: the handshake waits for the order, then succeeds with a certificate from the CA
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut s = connector
        .connect(
            rustls::pki_types::ServerName::try_from("shop.cloud.test").unwrap(),
            tcp,
        )
        .await
        .expect("a certificate was issued on demand");
    s.write_all(b"GET / HTTP/1.1\r\nHost: shop.cloud.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.starts_with("HTTP/1.1 200") && text.contains("hello from the app"),
        "{text}"
    );
    let stored = store.cert("shop.cloud.test").unwrap().expect("stored");
    assert!(stored.not_after > oc_store::now_ms());
    assert!(
        store
            .acme_account("https://127.0.0.1:14000/dir")
            .unwrap()
            .is_some(),
        "account kept for the next start"
    );
    // the next handshake reuses it
    let tcp = TcpStream::connect(addr).await.unwrap();
    connector
        .connect(
            rustls::pki_types::ServerName::try_from("shop.cloud.test").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    assert_eq!(
        store.cert("shop.cloud.test").unwrap().unwrap().cert_pem,
        stored.cert_pem
    );
}
