//! Host classification and route matching: pure functions over a snapshot of the store.

use std::collections::{HashMap, HashSet};

use oc_store::{GatewaySnapshot, Route};

/// What a `Host` header refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKind {
    /// The API and console.
    Console,
    /// `<project>.<base>`.
    Project(String),
    /// A verified custom domain of a project.
    Custom {
        /// The project.
        project: String,
    },
    /// Not ours.
    Unknown,
}

/// The routing tables, built from a [`GatewaySnapshot`].
#[derive(Clone, Debug, Default)]
pub struct Routing {
    /// The store generation this was built at.
    pub generation: u64,
    projects: HashSet<String>,
    domains: HashMap<String, String>,
    routes: HashMap<String, Vec<Route>>,
    ports: HashMap<String, u16>,
}

impl Routing {
    /// Builds the tables.
    #[must_use]
    pub fn from_snapshot(s: GatewaySnapshot) -> Self {
        let mut routes: HashMap<String, Vec<Route>> = HashMap::new();
        for r in s.routes {
            routes.entry(r.project.clone()).or_default().push(r);
        }
        Self {
            generation: s.generation,
            projects: s.projects.into_iter().collect(),
            domains: s.domains.into_iter().collect(),
            routes,
            ports: s.ports.into_iter().collect(),
        }
    }

    /// What `host` (already normalised) is.
    #[must_use]
    pub fn classify(&self, host: &str, base: &str) -> HostKind {
        if host == base || host.strip_prefix("console.") == Some(base) {
            return HostKind::Console;
        }
        if let Some(label) = host.strip_suffix(base).and_then(|h| h.strip_suffix('.'))
            && !label.contains('.')
            && self.projects.contains(label)
        {
            return HostKind::Project(label.to_owned());
        }
        match self.domains.get(host) {
            Some(p) => HostKind::Custom { project: p.clone() },
            None => HostKind::Unknown,
        }
    }

    /// The route for a request to `host` / `path` in `project`.
    #[must_use]
    pub fn route(&self, project: &str, host: &str, path: &str) -> Option<&Route> {
        match_route(self.routes.get(project)?, host, path)
    }

    /// The loopback port of an app.
    #[must_use]
    pub fn port(&self, app: &str) -> Option<u16> {
        self.ports.get(app).copied()
    }
}

/// Lower-cases a `Host` header value and strips the port and a trailing dot. `None` for something that cannot be a
/// host name.
#[must_use]
pub fn normalize_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        return None; // IPv6 literals are never a project host
    }
    let h = raw
        .split(':')
        .next()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let ok = !h.is_empty()
        && h.len() <= 253
        && h.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.');
    ok.then_some(h)
}

/// Whether `prefix` covers `path` on a segment boundary: `/api` covers `/api` and `/api/x`, not `/apix`.
#[must_use]
pub fn path_matches(prefix: &str, path: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    p.is_empty()
        || path == p
        || path
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The best route: the longest matching path prefix; at equal length a route limited to `host` beats one for all
/// hosts. Routes limited to another host never match.
#[must_use]
pub fn match_route<'a>(routes: &'a [Route], host: &str, path: &str) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| (r.host.is_empty() || r.host == host) && path_matches(&r.path_prefix, path))
        .max_by_key(|r| {
            (
                r.path_prefix.trim_end_matches('/').len(),
                !r.host.is_empty(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: i64, host: &str, prefix: &str, app: &str) -> Route {
        Route {
            id,
            project: "shop".into(),
            host: host.into(),
            path_prefix: prefix.into(),
            app: app.into(),
        }
    }

    fn routing() -> Routing {
        Routing::from_snapshot(GatewaySnapshot {
            generation: 3,
            projects: vec!["shop".into(), "blog".into()],
            domains: vec![("shop.kunde.de".into(), "shop".into())],
            routes: vec![
                route(1, "", "/", "shop.web"),
                route(2, "", "/api", "shop.api"),
                route(3, "shop.kunde.de", "/api", "shop.api-eu"),
            ],
            ports: vec![("shop.web".into(), 20000)],
        })
    }

    #[test]
    fn classifies_hosts() {
        let r = routing();
        let base = "cloud.example.com";
        assert_eq!(r.classify("cloud.example.com", base), HostKind::Console);
        assert_eq!(
            r.classify("console.cloud.example.com", base),
            HostKind::Console
        );
        assert_eq!(
            r.classify("shop.cloud.example.com", base),
            HostKind::Project("shop".into())
        );
        assert_eq!(
            r.classify("nope.cloud.example.com", base),
            HostKind::Unknown
        );
        assert_eq!(
            r.classify("a.shop.cloud.example.com", base),
            HostKind::Unknown,
            "one label only"
        );
        assert_eq!(r.classify("evilcloud.example.com", base), HostKind::Unknown);
        assert_eq!(
            r.classify("shop.kunde.de", base),
            HostKind::Custom {
                project: "shop".into()
            }
        );
        assert_eq!(r.classify("other.kunde.de", base), HostKind::Unknown);
    }

    #[test]
    fn longest_prefix_wins_and_host_specific_beats_general() {
        let r = routing();
        assert_eq!(
            r.route("shop", "shop.cloud.example.com", "/").unwrap().app,
            "shop.web"
        );
        assert_eq!(
            r.route("shop", "shop.cloud.example.com", "/x/y")
                .unwrap()
                .app,
            "shop.web"
        );
        assert_eq!(
            r.route("shop", "shop.cloud.example.com", "/api/v1")
                .unwrap()
                .app,
            "shop.api"
        );
        assert_eq!(
            r.route("shop", "shop.cloud.example.com", "/apix")
                .unwrap()
                .app,
            "shop.web"
        );
        assert_eq!(
            r.route("shop", "shop.kunde.de", "/api").unwrap().app,
            "shop.api-eu"
        );
        assert!(r.route("blog", "blog.cloud.example.com", "/").is_none());
        assert_eq!(r.port("shop.web"), Some(20000));
    }

    #[test]
    fn host_headers_are_normalised() {
        assert_eq!(
            normalize_host("Shop.Cloud.Example.com:8443").as_deref(),
            Some("shop.cloud.example.com")
        );
        assert_eq!(
            normalize_host("example.com.").as_deref(),
            Some("example.com")
        );
        assert_eq!(normalize_host("[::1]:80"), None);
        assert_eq!(normalize_host("bad host"), None);
        assert_eq!(normalize_host(""), None);
    }

    #[test]
    fn prefix_boundaries() {
        assert!(path_matches("/", "/anything"));
        assert!(path_matches("/api/", "/api"));
        assert!(path_matches("/api", "/api/"));
        assert!(!path_matches("/api", "/apix"));
    }
}
