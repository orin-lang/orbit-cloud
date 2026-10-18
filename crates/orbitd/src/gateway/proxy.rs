//! The reverse proxy: streams requests and responses between the gateway and an app on a loopback port, tunnels
//! upgrades (WebSockets), and sets the forwarding headers.

use std::time::Duration;

use axum::body::Body;
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::{HeaderMap, Request, Response, StatusCode, Uri};
use http_body_util::Limited;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::auth::RequestMeta;

/// The upstream client type.
pub type Client = HyperClient<HttpConnector, Body>;

/// Proxy limits.
#[derive(Clone, Copy, Debug)]
pub struct ProxyConfig {
    /// Largest accepted request body in bytes.
    pub max_body: u64,
    /// How long the app may take to answer with response headers.
    pub upstream_timeout: Duration,
}

/// A client for loopback upstreams (HTTP/1.1, pooled).
#[must_use]
pub fn client() -> Client {
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(Duration::from_secs(3)));
    http.set_nodelay(true);
    HyperClient::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .build(http)
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(h: &mut HeaderMap, keep_upgrade: bool) {
    let named: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for n in HOP_BY_HOP {
        if keep_upgrade && (n == "connection" || n == "upgrade") {
            continue;
        }
        h.remove(n);
    }
    if !keep_upgrade {
        for n in named {
            h.remove(n);
        }
    }
}

/// A plain error page.
#[must_use]
pub fn error_page(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(Body::from(format!(
        "{} {}\n{msg}\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

fn is_upgrade(h: &HeaderMap) -> bool {
    h.get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        && h.contains_key(header::UPGRADE)
}

fn limit_hit(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if err.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Forwards `req` to `127.0.0.1:port`. `host` and `scheme` are what the client used.
pub async fn forward(
    client: &Client,
    cfg: ProxyConfig,
    mut req: Request<Body>,
    port: u16,
    meta: &RequestMeta,
    host: &str,
    request_id: &str,
) -> Response<Body> {
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len > cfg.max_body
    {
        return error_page(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let (mut parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let Ok(uri) = format!("http://127.0.0.1:{port}{pq}").parse::<Uri>() else {
        return error_page(StatusCode::BAD_REQUEST, "bad request target");
    };
    parts.uri = uri;
    parts.version = axum::http::Version::HTTP_11;
    strip_hop_by_hop(&mut parts.headers, upgrade);
    let h = &mut parts.headers;
    let peer = meta.ip.map(|i| i.to_string()).unwrap_or_default();
    // Our own value replaces whatever the client sent (the gateway already decided whom to believe).
    h.remove("x-forwarded-for");
    h.remove("x-forwarded-proto");
    h.remove("x-forwarded-host");
    h.remove("forwarded");
    if let Ok(v) = HeaderValue::from_str(&peer) {
        h.insert("x-forwarded-for", v);
    }
    h.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if meta.https { "https" } else { "http" }),
    );
    if let Ok(v) = HeaderValue::from_str(host) {
        h.insert("x-forwarded-host", v);
    }
    if let Ok(v) = HeaderValue::from_str(request_id) {
        h.insert("x-request-id", v);
    }
    let body = Body::new(Limited::new(
        body,
        usize::try_from(cfg.max_body).unwrap_or(usize::MAX),
    ));
    let upstream_req = Request::from_parts(parts, body);
    let mut resp =
        match tokio::time::timeout(cfg.upstream_timeout, client.request(upstream_req)).await {
            Err(_) => {
                return error_page(
                    StatusCode::GATEWAY_TIMEOUT,
                    "the app did not answer in time",
                );
            }
            Ok(Err(e)) => {
                if limit_hit(&e) {
                    return error_page(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
                }
                tracing::debug!("upstream 127.0.0.1:{port}: {e}");
                return error_page(StatusCode::BAD_GATEWAY, "the app is not reachable");
            }
            Ok(Ok(r)) => r,
        };
    if upgrade && resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        if let Some(cu) = client_upgrade {
            tokio::spawn(async move {
                let (Ok(c), Ok(u)) = (cu.await, upstream_upgrade.await) else {
                    return;
                };
                let mut c = TokioIo::new(c);
                let mut u = TokioIo::new(u);
                let _ = tokio::io::copy_bidirectional(&mut c, &mut u).await;
            });
        }
        let mut out = Response::new(Body::empty());
        *out.status_mut() = resp.status();
        *out.headers_mut() = resp.headers().clone();
        return out;
    }
    let (mut rparts, rbody) = resp.into_parts();
    strip_hop_by_hop(&mut rparts.headers, false);
    if let Ok(v) = HeaderValue::from_str(request_id) {
        rparts.headers.insert("x-request-id", v);
    }
    Response::from_parts(rparts, Body::new(rbody))
}
