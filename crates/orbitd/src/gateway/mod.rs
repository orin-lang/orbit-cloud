//! The public gateway: one HTTP and one HTTPS listener that route by `Host` to the console, to the apps of a project
//! (reverse proxy) and to custom domains, with certificates for known hosts only. See `docs/platform.md`.

pub mod accesslog;
pub mod dns;
pub mod domains;
pub mod forwarded;
pub mod hosts;
pub mod proxy;
pub mod tls;

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use oc_store::Store;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

use self::forwarded::TrustedProxies;
use self::hosts::{HostKind, Routing, normalize_host};
use self::proxy::{ProxyConfig, error_page};
use self::tls::{Challenges, KnownHost, TlsManager};
use crate::auth::RequestMeta;

/// Gateway settings.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// The operator's base domain.
    pub base_domain: String,
    /// Port of the HTTPS listener, for redirects (443 when absent).
    pub https_port: Option<u16>,
    /// Whether HTTP requests are redirected to HTTPS (TLS is on).
    pub redirect_to_https: bool,
    /// Peers whose `X-Forwarded-*` are believed.
    pub trusted: TrustedProxies,
    /// Proxy limits.
    pub proxy: ProxyConfig,
}

impl GatewayConfig {
    /// Settings for tests and plain-HTTP use.
    #[must_use]
    pub fn plain(base_domain: &str) -> Self {
        Self {
            base_domain: base_domain.to_owned(),
            https_port: None,
            redirect_to_https: false,
            trusted: TrustedProxies::default(),
            proxy: ProxyConfig {
                max_body: 100 * 1024 * 1024,
                upstream_timeout: Duration::from_secs(60),
            },
        }
    }
}

/// The gateway.
pub struct Gateway {
    store: Arc<Store>,
    cfg: GatewayConfig,
    routing: RwLock<Arc<Routing>>,
    console: Router,
    client: proxy::Client,
    access: tokio::sync::mpsc::UnboundedSender<accesslog::Access>,
    challenges: Arc<Challenges>,
    tls: Option<Arc<TlsManager>>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("base", &self.cfg.base_domain)
            .finish_non_exhaustive()
    }
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

fn sane_request_id(v: &HeaderValue) -> Option<String> {
    let s = v.to_str().ok()?;
    (!s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
    .then(|| s.to_owned())
}

impl Gateway {
    /// A gateway serving `console` (the API router) and the apps found in `store`. Must be called inside a Tokio
    /// runtime (it starts the access-log writer).
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        console: Router,
        cfg: GatewayConfig,
        tls: Option<Arc<TlsManager>>,
        challenges: Arc<Challenges>,
    ) -> Arc<Self> {
        let access = accesslog::spawn_writer(Arc::clone(&store));
        Arc::new(Self {
            store,
            cfg,
            routing: RwLock::new(Arc::new(Routing::default())),
            console,
            client: proxy::client(),
            access,
            challenges,
            tls,
        })
    }

    /// The current routing tables (rebuilt when the store changed).
    #[must_use]
    pub fn routing(&self) -> Arc<Routing> {
        let generation = self.store.generation();
        {
            let r = self
                .routing
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if r.generation == generation && generation != 0 && r.generation != 0 {
                return Arc::clone(&r);
            }
        }
        match self.store.gateway_snapshot() {
            Ok(s) => {
                let fresh = Arc::new(Routing::from_snapshot(s));
                *self
                    .routing
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&fresh);
                fresh
            }
            Err(e) => {
                tracing::error!("gateway: cannot read the routing tables: {e}");
                Arc::clone(
                    &self
                        .routing
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
            }
        }
    }

    /// Whether `host` is one the platform serves.
    #[must_use]
    pub fn is_known(&self, host: &str) -> bool {
        KnownHost::check(&self.routing(), &self.cfg.base_domain, host).is_some()
    }

    /// Handles one request that arrived from `peer`, over TLS or not.
    pub async fn handle(
        self: &Arc<Self>,
        mut req: Request<Body>,
        peer: IpAddr,
        tls: bool,
    ) -> Response<Body> {
        let Some(host) = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()))
            .and_then(|h| normalize_host(&h))
        else {
            return error_page(StatusCode::BAD_REQUEST, "missing or invalid Host header");
        };
        let meta = self.cfg.trusted.resolve(peer, req.headers(), tls);
        let path = req.uri().path().to_owned();

        if !tls && let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
            return match self.challenges.get(token) {
                Some(v) => {
                    let mut r = Response::new(Body::from(v));
                    r.headers_mut()
                        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                    r
                }
                None => error_page(StatusCode::NOT_FOUND, "no such challenge"),
            };
        }

        let routing = self.routing();
        let kind = routing.classify(&host, &self.cfg.base_domain);
        if kind == HostKind::Unknown {
            return error_page(
                StatusCode::NOT_FOUND,
                "no project is served under this host name",
            );
        }
        if self.cfg.redirect_to_https && !meta.https {
            let port = self
                .cfg
                .https_port
                .filter(|p| *p != 443)
                .map_or_else(String::new, |p| format!(":{p}"));
            let target = format!(
                "https://{host}{port}{}",
                req.uri()
                    .path_and_query()
                    .map_or("/", axum::http::uri::PathAndQuery::as_str)
            );
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::PERMANENT_REDIRECT;
            if let Ok(v) = HeaderValue::from_str(&target) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            return r;
        }

        match kind {
            HostKind::Console => {
                req.extensions_mut().insert(meta);
                let mut resp = match self.console.clone().oneshot(req).await {
                    Ok(r) => r,
                    Err(e) => match e {},
                };
                if meta.https {
                    resp.headers_mut().insert(
                        header::STRICT_TRANSPORT_SECURITY,
                        HeaderValue::from_static("max-age=31536000"),
                    );
                }
                resp
            }
            HostKind::Project(project) | HostKind::Custom { project } => {
                self.proxy_request(&routing, &project, &host, req, meta)
                    .await
            }
            HostKind::Unknown => error_page(StatusCode::NOT_FOUND, "unknown host"),
        }
    }

    async fn proxy_request(
        &self,
        routing: &Routing,
        project: &str,
        host: &str,
        req: Request<Body>,
        meta: RequestMeta,
    ) -> Response<Body> {
        let path = req.uri().path().to_owned();
        let Some(route) = routing.route(project, host, &path) else {
            return error_page(
                StatusCode::NOT_FOUND,
                "this project has no route for this path",
            );
        };
        let Some(port) = routing.port(&route.app) else {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "the app behind this route is not an HTTP service",
            );
        };
        let request_id = req
            .headers()
            .get("x-request-id")
            .and_then(sane_request_id)
            .unwrap_or_else(|| oc_store::random_id(8));
        let method = req.method().to_string();
        let start_us = now_us();
        let resp = proxy::forward(
            &self.client,
            self.cfg.proxy,
            req,
            port,
            &meta,
            host,
            &request_id,
        )
        .await;
        let bytes = resp
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let _ = self.access.send(accesslog::Access {
            app: route.app.clone(),
            host: host.to_owned(),
            method,
            path,
            status: resp.status().as_u16(),
            start_us,
            end_us: now_us(),
            bytes,
            ip: meta.ip,
            request_id,
        });
        resp
    }

    /// Serves one accepted connection.
    async fn serve_io<I>(self: Arc<Self>, io: I, peer: SocketAddr, tls: bool)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let gw = Arc::clone(&self);
        let svc = service_fn(move |req: Request<Incoming>| {
            let gw = Arc::clone(&gw);
            async move { Ok::<_, Infallible>(gw.handle(req.map(Body::new), peer.ip(), tls).await) }
        });
        let mut b = auto::Builder::new(TokioExecutor::new());
        b.http1()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30));
        b.http2()
            .timer(TokioTimer::new())
            .keep_alive_interval(Some(Duration::from_secs(30)))
            .keep_alive_timeout(Duration::from_secs(20));
        if let Err(e) = b
            .serve_connection_with_upgrades(TokioIo::new(io), svc)
            .await
        {
            tracing::debug!("connection from {peer}: {e}");
        }
    }

    /// Accepts plain HTTP connections forever.
    pub async fn serve_http(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    tokio::spawn(Arc::clone(&self).serve_io(stream, peer, false));
                }
                Err(e) => {
                    tracing::warn!("http accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Accepts HTTPS connections forever. The certificate is chosen after reading the ClientHello, and only for
    /// known hosts; any other server name (or none) is dropped without a handshake.
    pub async fn serve_https(self: Arc<Self>, listener: TcpListener) {
        let Some(tls) = self.tls.clone() else {
            tracing::error!("https listener without a TLS manager");
            return;
        };
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    let gw = Arc::clone(&self);
                    let tls = Arc::clone(&tls);
                    tokio::spawn(async move {
                        let wait = tls.issue_wait + Duration::from_secs(15);
                        let accepted = tokio::time::timeout(wait, async {
                            let start = tokio_rustls::LazyConfigAcceptor::new(
                                rustls::server::Acceptor::default(),
                                stream,
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                            let sni = start.client_hello().server_name().and_then(normalize_host);
                            let known = sni
                                .as_deref()
                                .and_then(|h| {
                                    KnownHost::check(&gw.routing(), &gw.cfg.base_domain, h)
                                })
                                .ok_or_else(|| format!("unknown server name {sni:?}"))?;
                            let config = tls.config_for(&known).await?;
                            start.into_stream(config).await.map_err(|e| e.to_string())
                        })
                        .await;
                        match accepted {
                            Ok(Ok(stream)) => gw.serve_io(stream, peer, true).await,
                            Ok(Err(e)) => tracing::debug!("tls from {peer}: {e}"),
                            Err(_) => tracing::debug!("tls from {peer}: handshake timed out"),
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("https accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Background work: proactive certificates for newly verified domains, and renewals.
    pub fn spawn_background(
        self: &Arc<Self>,
        verified: tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let Some(tls) = self.tls.clone() else { return };
        let mut rx = verified;
        let t2 = Arc::clone(&tls);
        tokio::spawn(async move {
            while let Some(host) = rx.recv().await {
                let _ = t2.obtain(&host).await;
            }
        });
        let gw = Arc::clone(self);
        tokio::spawn(tls.renew_loop(move |h| gw.is_known(h)));
    }
}

/// The TLS-related command-line settings, resolved.
#[derive(Clone, Debug, Default)]
pub struct TlsSettings {
    /// Operator certificate chain.
    pub cert: Option<PathBuf>,
    /// Operator key.
    pub key: Option<PathBuf>,
    /// ACME contact email; ACME is on when set.
    pub acme_email: Option<String>,
    /// ACME directory URL.
    pub acme_directory: String,
    /// Extra root certificate for the ACME directory's TLS.
    pub acme_ca_file: Option<PathBuf>,
}

/// Builds the TLS manager (and the challenge table the HTTP listener serves).
///
/// # Errors
/// Unreadable wildcard files.
pub fn build_tls(
    store: &Arc<Store>,
    s: &TlsSettings,
) -> Result<(Arc<TlsManager>, Arc<Challenges>), String> {
    let challenges = Arc::new(Challenges::default());
    let wildcard = match (&s.cert, &s.key) {
        (Some(c), Some(k)) => Some(tls::Wildcard::load(c, k)?),
        (None, None) => None,
        _ => return Err("--tls-cert and --tls-key go together".into()),
    };
    let issuer: Option<Arc<dyn tls::CertIssuer>> = s.acme_email.as_ref().map(|email| {
        let a = Arc::new(tls::AcmeIssuer::new(
            Arc::clone(store),
            email.clone(),
            s.acme_directory.clone(),
            s.acme_ca_file.clone(),
            Arc::clone(&challenges),
        ));
        Arc::new(a) as Arc<dyn tls::CertIssuer>
    });
    Ok((
        Arc::new(TlsManager::new(Arc::clone(store), wildcard, issuer)),
        challenges,
    ))
}
