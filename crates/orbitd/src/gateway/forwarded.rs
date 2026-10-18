//! Which peers may tell us the client address and scheme (`X-Forwarded-*`), and the headers we send upstream.

use std::net::IpAddr;

use axum::http::HeaderMap;
use ipnet::IpNet;

use crate::auth::RequestMeta;

/// Peers whose `X-Forwarded-*` headers are believed.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(pub Vec<IpNet>);

impl TrustedProxies {
    /// Parses CIDRs or bare addresses.
    ///
    /// # Errors
    /// The first entry that does not parse.
    pub fn parse(items: &[String]) -> Result<Self, String> {
        items
            .iter()
            .map(|s| {
                s.parse::<IpNet>()
                    .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
                    .map_err(|_| format!("bad trusted proxy `{s}`: use an address or CIDR"))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|n| n.contains(&ip))
    }

    /// The client address and scheme of a request from `peer`, over TLS or not. From an untrusted peer the headers are
    /// ignored. From a trusted one the client is the right-most `X-Forwarded-For` entry that is not itself trusted.
    #[must_use]
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap, tls: bool) -> RequestMeta {
        if !self.trusts(peer) {
            return RequestMeta {
                ip: Some(peer),
                https: tls,
            };
        }
        let ip = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|p| p.trim().parse::<IpAddr>().ok())
            .rev()
            .find(|ip| !self.trusts(*ip))
            .unwrap_or(peer);
        let https = tls
            || headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("https"));
        RequestMeta {
            ip: Some(ip),
            https,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn untrusted_peers_cannot_spoof() {
        let t = TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap();
        let m = t.resolve(
            "203.0.113.9".parse().unwrap(),
            &hm(&[
                ("x-forwarded-for", "1.2.3.4"),
                ("x-forwarded-proto", "https"),
            ]),
            false,
        );
        assert_eq!(m.ip, Some("203.0.113.9".parse().unwrap()));
        assert!(!m.https);
    }

    #[test]
    fn trusted_peers_are_believed_from_the_right() {
        let t = TrustedProxies::parse(&["10.0.0.0/8".into(), "192.168.1.1".into()]).unwrap();
        let m = t.resolve(
            "10.1.1.1".parse().unwrap(),
            &hm(&[
                ("x-forwarded-for", "6.6.6.6, 198.51.100.4, 10.2.2.2"),
                ("x-forwarded-proto", "https"),
            ]),
            false,
        );
        assert_eq!(
            m.ip,
            Some("198.51.100.4".parse().unwrap()),
            "the spoofed left entry is ignored"
        );
        assert!(m.https);
        assert!(TrustedProxies::parse(&["nonsense".into()]).is_err());
    }
}
