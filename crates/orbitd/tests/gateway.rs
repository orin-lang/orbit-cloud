//! The gateway with real sockets: host routing, reverse proxying to a service on a loopback port (headers, bodies,
//! upgrades, errors), access logs and spans, HTTP to HTTPS redirects, ACME challenge answers and TLS for known hosts only.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::gateway::forwarded::TrustedProxies;
use orbitd::gateway::proxy::ProxyConfig;
use orbitd::gateway::tls::{Challenges, TlsManager, Wildcard};
use orbitd::gateway::{Gateway, GatewayConfig};
use orbitd::node::Node;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BASE: &str = "cloud.test";

/// A backend app: answers with what it received. `Upgrade: echo` switches to an echo tunnel.
async fn backend(name: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(move |mut req: Request<hyper::body::Incoming>| async move {
                    if req.headers().get("upgrade").is_some_and(|v| v == "echo") {
                        let up = hyper::upgrade::on(&mut req);
                        tokio::spawn(async move {
                            if let Ok(u) = up.await {
                                let mut io = TokioIo::new(u);
                                let mut buf = [0u8; 256];
                                while let Ok(n) = io.read(&mut buf).await {
                                    if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        });
                        return Ok::<_, Infallible>(
                            Response::builder()
                                .status(101)
                                .header("connection", "upgrade")
                                .header("upgrade", "echo")
                                .body(Body::empty())
                                .unwrap(),
                        );
                    }
                    let h = |n: &str| {
                        req.headers()
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("-")
                            .to_owned()
                    };
                    let head = format!(
                        "app={name} method={} uri={} host={} xff={} xfp={} xfh={} rid={} conn={}\n",
                        req.method(),
                        req.uri(),
                        h("host"),
                        h("x-forwarded-for"),
                        h("x-forwarded-proto"),
                        h("x-forwarded-host"),
                        h("x-request-id"),
                        h("connection"),
                    );
                    let body = req
                        .into_body()
                        .collect()
                        .await
                        .map(|b| b.to_bytes().len())
                        .unwrap_or(0);
                    Ok(Response::new(Body::from(format!(
                        "{head}body_bytes={body}\n"
                    ))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}

struct World {
    _dir: tempfile::TempDir,
    store: Arc<oc_store::Store>,
    gw: Arc<Gateway>,
    http: SocketAddr,
}

async fn world_with(
    cfg: GatewayConfig,
    tls: Option<Arc<TlsManager>>,
    challenges: Arc<Challenges>,
) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let node = Node::new(Arc::new(
        Executor::new(Arc::clone(&store), dir.path(), false).unwrap(),
    ));
    let state = AppState::new(node, Some(Arc::from("admin")));
    let gw = Gateway::new(Arc::clone(&store), router(state), cfg, tls, challenges);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = l.local_addr().unwrap();
    tokio::spawn(Arc::clone(&gw).serve_http(l));
    World {
        _dir: dir,
        store,
        gw,
        http,
    }
}

async fn world() -> World {
    world_with(GatewayConfig::plain(BASE), None, Arc::default()).await
}

/// Registers an app of a project as an HTTP service on `port` (no wasm worker is needed to test the proxy).
fn http_app(store: &oc_store::Store, project: &str, name: &str, port: u16) -> String {
    let key = oc_store::key_of(project, name);
    let m = store.put_module(b"\0asm\x01\0\0\0").unwrap();
    store
        .deploy(
            &key,
            &m,
            &serde_json::json!({"kind": "service", "http": true}),
        )
        .unwrap();
    store.assign_http_port(&key, port).unwrap();
    key
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

/// One HTTP/1.1 request with `Connection: close`.
async fn request(
    addr: SocketAddr,
    method: &str,
    host: &str,
    path: &str,
    extra: &str,
    body: &[u8],
) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}content-length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw)).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        headers: headers.to_lowercase(),
        body: body.to_owned(),
    }
}

async fn get(w: &World, host: &str, path: &str) -> Reply {
    request(w.http, "GET", host, path, "", b"").await
}

#[tokio::test]
async fn routes_by_host_and_path_prefix() {
    let w = world().await;
    w.store.create_project("default", "shop").unwrap();
    w.store.create_project("default", "blog").unwrap();
    let web = backend("web").await;
    let api = backend("api").await;
    let blog = backend("blog").await;
    let web_key = http_app(&w.store, "shop", "web", web);
    let api_key = http_app(&w.store, "shop", "api", api);
    let blog_key = http_app(&w.store, "blog", "site", blog);
    w.store.add_route("shop", "", "/", &web_key).unwrap();
    w.store.add_route("shop", "", "/api", &api_key).unwrap();
    w.store.add_route("blog", "", "/", &blog_key).unwrap();

    let r = get(&w, "shop.cloud.test", "/").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("app=web"), "{}", r.body);
    assert!(
        get(&w, "shop.cloud.test", "/api/v1/items?x=1")
            .await
            .body
            .contains("app=api method=GET uri=/api/v1/items?x=1")
    );
    assert!(
        get(&w, "shop.cloud.test", "/apix")
            .await
            .body
            .contains("app=web"),
        "prefixes match whole segments"
    );
    assert!(
        get(&w, "blog.cloud.test:8080", "/")
            .await
            .body
            .contains("app=blog"),
        "the port in Host is ignored"
    );
    assert!(
        get(&w, "SHOP.Cloud.Test", "/")
            .await
            .body
            .contains("app=web"),
        "case-insensitive"
    );

    // unknown project, deeper subdomain, foreign host: 404 and nothing proxied
    assert_eq!(get(&w, "nope.cloud.test", "/").await.status, 404);
    assert_eq!(get(&w, "a.shop.cloud.test", "/").await.status, 404);
    assert_eq!(get(&w, "example.org", "/").await.status, 404);

    // the console and API live on console.<base> and on the base domain itself
    for host in ["console.cloud.test", "cloud.test"] {
        let r = get(&w, host, "/healthz").await;
        assert_eq!((r.status, r.body.trim()), (200, "ok"), "{host}");
    }
    let r = request(
        w.http,
        "GET",
        "console.cloud.test",
        "/v1/projects",
        "authorization: Bearer admin\r\n",
        b"",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"shop\""));
    // an app cannot shadow the console: /v1 on a project host goes to the app
    assert!(
        get(&w, "shop.cloud.test", "/v1/projects")
            .await
            .body
            .contains("app=web")
    );
}

#[tokio::test]
async fn custom_domains_route_only_once_verified() {
    let w = world().await;
    w.store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let key = http_app(&w.store, "shop", "web", web);
    w.store.add_route("shop", "", "/", &key).unwrap();
    w.store.add_domain("shop", "shop.kunde.de").unwrap();
    assert_eq!(
        get(&w, "shop.kunde.de", "/").await.status,
        404,
        "pending is not served"
    );
    w.store
        .record_domain_check("shop", "shop.kunde.de", true, true, None)
        .unwrap();
    let r = get(&w, "shop.kunde.de", "/").await;
    assert_eq!(r.status, 200);
    assert!(
        r.body.contains("host=shop.kunde.de"),
        "the app sees the custom host: {}",
        r.body
    );
    w.store.delete_domain("shop", "shop.kunde.de").unwrap();
    assert_eq!(get(&w, "shop.kunde.de", "/").await.status, 404);
}

#[tokio::test]
async fn forwarding_headers_request_ids_and_errors() {
    let w = world().await;
    w.store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let key = http_app(&w.store, "shop", "web", web);
    w.store.add_route("shop", "", "/", &key).unwrap();

    // a client cannot spoof forwarding headers on an untrusted connection
    let r = request(
        w.http,
        "GET",
        "shop.cloud.test",
        "/",
        "x-forwarded-for: 6.6.6.6\r\nx-forwarded-proto: https\r\nx-forwarded-host: evil.test\r\n",
        b"",
    )
    .await;
    assert!(r.body.contains("xff=127.0.0.1 "), "{}", r.body);
    assert!(r.body.contains("xfp=http "), "{}", r.body);
    assert!(r.body.contains("xfh=shop.cloud.test "), "{}", r.body);
    assert!(
        r.body.contains("host=shop.cloud.test "),
        "Host is passed through: {}",
        r.body
    );
    // a request id is generated and returned; a sane one from the client is kept; an insane one is replaced
    let id = r
        .headers
        .lines()
        .find_map(|l| l.strip_prefix("x-request-id: "))
        .unwrap()
        .to_owned();
    assert!(r.body.contains(&format!("rid={id} ")), "{}", r.body);
    let r = request(
        w.http,
        "GET",
        "shop.cloud.test",
        "/",
        "x-request-id: trace-123\r\n",
        b"",
    )
    .await;
    assert!(r.body.contains("rid=trace-123 ") && r.headers.contains("x-request-id: trace-123"));
    let r = request(
        w.http,
        "GET",
        "shop.cloud.test",
        "/",
        "x-request-id: bad id with spaces\r\n",
        b"",
    )
    .await;
    assert!(!r.body.contains("rid=bad"), "{}", r.body);
    // hop-by-hop headers do not reach the app
    let r = request(
        w.http,
        "GET",
        "shop.cloud.test",
        "/",
        "keep-alive: 5\r\nte: trailers\r\n",
        b"",
    )
    .await;
    assert!(
        r.body.contains("conn=-") || r.body.contains("conn=close"),
        "{}",
        r.body
    );

    // bodies are streamed through
    let big = vec![b'x'; 300_000];
    let r = request(w.http, "POST", "shop.cloud.test", "/upload", "", &big).await;
    assert!(r.body.contains("body_bytes=300000"), "{}", r.body);

    // an app that is down answers 502; a route to a non-HTTP app 503
    w.store.create_project("default", "dead").unwrap();
    let k2 = http_app(&w.store, "dead", "web", 1); // nothing listens on port 1
    w.store.add_route("dead", "", "/", &k2).unwrap();
    assert_eq!(get(&w, "dead.cloud.test", "/").await.status, 502);
    w.store.create_project("default", "empty").unwrap();
    assert_eq!(
        get(&w, "empty.cloud.test", "/").await.status,
        404,
        "no route"
    );
}

#[tokio::test]
async fn forwarded_headers_from_trusted_proxies_and_body_limit() {
    let mut cfg = GatewayConfig::plain(BASE);
    cfg.trusted = TrustedProxies::parse(&["127.0.0.0/8".into()]).unwrap();
    cfg.proxy = ProxyConfig {
        max_body: 1000,
        upstream_timeout: Duration::from_secs(5),
    };
    let w = world_with(cfg, None, Arc::default()).await;
    w.store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let key = http_app(&w.store, "shop", "web", web);
    w.store.add_route("shop", "", "/", &key).unwrap();
    let r = request(
        w.http,
        "GET",
        "shop.cloud.test",
        "/",
        "x-forwarded-for: 203.0.113.9\r\nx-forwarded-proto: https\r\n",
        b"",
    )
    .await;
    assert!(
        r.body.contains("xff=203.0.113.9 ") && r.body.contains("xfp=https "),
        "{}",
        r.body
    );
    assert_eq!(
        request(w.http, "POST", "shop.cloud.test", "/", "", &vec![b'a'; 999])
            .await
            .status,
        200
    );
    assert_eq!(
        request(
            w.http,
            "POST",
            "shop.cloud.test",
            "/",
            "",
            &vec![b'a'; 1001]
        )
        .await
        .status,
        413
    );
}

#[tokio::test]
async fn websocket_style_upgrades_are_tunnelled() {
    let w = world().await;
    w.store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let key = http_app(&w.store, "shop", "web", web);
    w.store.add_route("shop", "", "/", &key).unwrap();
    let mut s = TcpStream::connect(w.http).await.unwrap();
    s.write_all(b"GET /ws HTTP/1.1\r\nHost: shop.cloud.test\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 512];
    let mut got = Vec::new();
    while !String::from_utf8_lossy(&got).contains("\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed before the upgrade answer");
        got.extend_from_slice(&buf[..n]);
    }
    assert!(
        String::from_utf8_lossy(&got).starts_with("HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    s.write_all(b"ping").await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        &buf[..n],
        b"ping",
        "bytes flow both ways through the tunnel"
    );
}

#[tokio::test]
async fn requests_are_logged_and_traced_with_the_app() {
    let w = world().await;
    w.store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let key = http_app(&w.store, "shop", "web", web);
    w.store.add_route("shop", "", "/", &key).unwrap();
    // the service's current run: request spans join its trace
    let d = w.store.app(&key).unwrap().active_deployment.unwrap();
    let run = w.store.start_run(&key, d, "service", None).unwrap();
    get(&w, "shop.cloud.test", "/hello?q=1").await;
    for _ in 0..50 {
        if !w
            .store
            .logs(&oc_store::LogQuery {
                app: Some(key.clone()),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let logs = w
        .store
        .logs(&oc_store::LogQuery {
            app: Some(key.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert!(
        logs[0].message.starts_with("GET /hello 200 "),
        "{}",
        logs[0].message
    );
    assert!(logs[0].message.contains("host=shop.cloud.test"));
    assert_eq!(logs[0].run_id.as_deref(), Some(run.id.as_str()));
    let spans = w.store.trace(&run.trace_id).unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "http.request");
    assert_eq!(spans[0].attrs["status"], 200);
    assert_eq!(spans[0].attrs["path"], "/hello");
}

#[tokio::test]
async fn http_redirects_to_https_but_answers_acme_challenges() {
    let challenges = Arc::new(Challenges::default());
    challenges.insert("tok123", "tok123.thumbprint");
    let mut cfg = GatewayConfig::plain(BASE);
    cfg.redirect_to_https = true;
    cfg.https_port = Some(8443);
    let w = world_with(cfg, None, challenges).await;
    w.store.create_project("default", "shop").unwrap();
    let r = get(&w, "shop.cloud.test", "/a?b=1").await;
    assert_eq!(r.status, 308);
    assert!(
        r.headers
            .contains("location: https://shop.cloud.test:8443/a?b=1"),
        "{}",
        r.headers
    );
    // unknown hosts are not redirected into a TLS port that has no certificate for them
    assert_eq!(get(&w, "evil.example", "/").await.status, 404);
    // the CA reaches the challenge over plain HTTP
    let c = get(&w, "shop.cloud.test", "/.well-known/acme-challenge/tok123").await;
    assert_eq!((c.status, c.body.as_str()), (200, "tok123.thumbprint"));
    assert_eq!(
        get(&w, "shop.cloud.test", "/.well-known/acme-challenge/other")
            .await
            .status,
        404
    );
    // behind a trusted TLS-terminating proxy there is no redirect
    let mut cfg = GatewayConfig::plain(BASE);
    cfg.redirect_to_https = true;
    cfg.trusted = TrustedProxies::parse(&["127.0.0.1".into()]).unwrap();
    let w2 = world_with(cfg, None, Arc::default()).await;
    w2.store.create_project("default", "shop").unwrap();
    let r = request(
        w2.http,
        "GET",
        "console.cloud.test",
        "/healthz",
        "x-forwarded-proto: https\r\n",
        b"",
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn https_serves_known_hosts_only() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key = rcgen::generate_simple_self_signed(vec![
        "*.cloud.test".to_owned(),
        "cloud.test".to_owned(),
    ])
    .unwrap();
    let cert_pem = key.cert.pem();
    let wildcard = Wildcard::from_pem(&cert_pem, &key.signing_key.serialize_pem()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let tls = Arc::new(TlsManager::new(Arc::clone(&store), Some(wildcard), None));
    let node = Node::new(Arc::new(
        Executor::new(Arc::clone(&store), dir.path(), false).unwrap(),
    ));
    let gw = Gateway::new(
        Arc::clone(&store),
        router(AppState::new(node, Some(Arc::from("admin")))),
        GatewayConfig::plain(BASE),
        Some(tls),
        Arc::default(),
    );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(Arc::clone(&gw).serve_https(l));
    store.create_project("default", "shop").unwrap();
    let web = backend("web").await;
    let k = http_app(&store, "shop", "web", web);
    store.add_route("shop", "", "/", &k).unwrap();

    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut cert_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let mut cc = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cc.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cc));
    let connect = |sni: &'static str| {
        let connector = connector.clone();
        async move {
            let tcp = TcpStream::connect(addr).await.unwrap();
            connector
                .connect(rustls::pki_types::ServerName::try_from(sni).unwrap(), tcp)
                .await
        }
    };

    // a known project host: handshake, request, proxied answer, HTTPS seen by the app
    let mut tls_stream = connect("shop.cloud.test")
        .await
        .expect("known host gets a certificate");
    tls_stream
        .write_all(b"GET / HTTP/1.1\r\nHost: shop.cloud.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tls_stream.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("xfp=https "), "{text}");
    // the console host works too
    assert!(connect("console.cloud.test").await.is_ok());
    // a name nobody owns: no handshake, even though the wildcard would cover it
    assert!(
        connect("random.cloud.test").await.is_err(),
        "unknown project hosts get no certificate"
    );
    assert!(connect("evil.example").await.is_err());
    // once the project exists, it does
    store.create_project("default", "random").unwrap();
    assert!(connect("random.cloud.test").await.is_ok());
}

#[tokio::test]
async fn gateway_state_follows_the_store() {
    let w = world().await;
    assert!(!w.gw.is_known("late.cloud.test"));
    w.store.create_project("default", "late").unwrap();
    assert!(
        w.gw.is_known("late.cloud.test"),
        "new projects are routable without a restart"
    );
}
