//! Custom domains: host validation, DNS verification and the background verifier.

use std::sync::Arc;
use std::time::Duration;

use oc_store::{Domain, Store};
use tokio::sync::mpsc;

use super::dns::DnsLookup;

/// The name of the TXT record that proves ownership of `host`.
#[must_use]
pub fn txt_name(host: &str) -> String {
    format!("_orbit-verify.{host}")
}

/// Normalises and validates a custom domain: lower case, no port, a DNS name with at least two labels, not an IP
/// address, not under the platform's base domain.
///
/// # Errors
/// A message for the caller.
pub fn validate_domain(raw: &str, base: Option<&str>) -> Result<String, String> {
    let host = raw.trim().trim_end_matches('.').to_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err("a host name is 1 to 253 characters".into());
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err("an IP address is not a domain".into());
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err("a custom domain needs at least two labels, like shop.example.org".into());
    }
    for l in &labels {
        let ok = (1..=63).contains(&l.len())
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if !ok {
            return Err(format!(
                "invalid label `{l}` (letters, digits and -; use the punycode form for IDNs)"
            ));
        }
    }
    if labels
        .last()
        .is_some_and(|t| t.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err("the top-level label cannot be all digits".into());
    }
    if let Some(base) = base
        && (host == base || host.ends_with(&format!(".{base}")))
    {
        return Err(format!(
            "`{host}` is under the platform domain `{base}`; project hosts need no custom domain"
        ));
    }
    Ok(host)
}

/// The outcome of one DNS check.
#[derive(Debug, PartialEq, Eq)]
pub struct Check {
    /// The TXT record matched.
    pub verified: bool,
    /// The host resolves to the same addresses as the project host.
    pub cname_ok: bool,
    /// Why not.
    pub error: Option<String>,
}

/// Checks one domain against DNS.
pub async fn check(dns: &dyn DnsLookup, base: Option<&str>, d: &Domain) -> Check {
    let name = txt_name(&d.host);
    let (verified, mut error) = match dns.txt(&name).await {
        Ok(records) => {
            let ok = records
                .iter()
                .any(|r| r.trim().trim_matches('"') == d.txt_value);
            (
                ok,
                (!ok).then(|| {
                    if records.is_empty() {
                        format!("no TXT record at {name} yet")
                    } else {
                        format!("the TXT record at {name} does not have the expected value")
                    }
                }),
            )
        }
        Err(e) => (false, Some(format!("DNS lookup of {name} failed: {e}"))),
    };
    let cname_ok = match base {
        Some(base) => {
            let target = format!("{}.{base}", d.project);
            let (a, b) = tokio::join!(dns.addrs(&d.host), dns.addrs(&target));
            match (a, b) {
                (Ok(a), Ok(b)) => a.iter().any(|x| b.contains(x)),
                _ => false,
            }
        }
        None => false,
    };
    if verified {
        error = None;
    }
    Check {
        verified,
        cname_ok,
        error,
    }
}

/// Checks a domain and records the result; returns whether it is verified now.
///
/// # Errors
/// Store failures.
pub async fn verify_and_record(
    store: &Store,
    dns: &dyn DnsLookup,
    base: Option<&str>,
    d: &Domain,
) -> Result<bool, oc_store::StoreError> {
    let c = check(dns, base, d).await;
    store.record_domain_check(
        &d.project,
        &d.host,
        c.verified,
        c.cname_ok,
        c.error.as_deref(),
    )?;
    Ok(c.verified)
}

/// How long to wait before checking a pending domain again: often at first, then less.
fn recheck_after(d: &Domain, now_ms: i64) -> i64 {
    let age = now_ms - d.created_at;
    if age < 3_600_000 {
        25_000
    } else if age < 86_400_000 {
        300_000
    } else {
        1_800_000
    }
}

/// Background task: every few seconds checks the pending domains that are due; sends each newly verified host on
/// `verified` so a certificate can be ordered.
pub async fn verifier_loop(
    store: Arc<Store>,
    dns: Arc<dyn DnsLookup>,
    base: Option<String>,
    verified: mpsc::UnboundedSender<String>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        let now = oc_store::now_ms();
        let Ok(pending) = store.pending_domains() else {
            continue;
        };
        for d in pending {
            if d.last_checked
                .is_some_and(|l| now - l < recheck_after(&d, now))
            {
                continue;
            }
            match verify_and_record(&store, dns.as_ref(), base.as_deref(), &d).await {
                Ok(true) => {
                    tracing::info!("domain {} verified for project {}", d.host, d.project);
                    let _ = verified.send(d.host.clone());
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("domain check of {}: {e}", d.host),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::dns::StaticDns;

    #[test]
    fn domain_validation() {
        assert_eq!(
            validate_domain(" Shop.Kunde.DE. ", Some("cloud.example.com")).unwrap(),
            "shop.kunde.de"
        );
        assert!(validate_domain("localhost", None).is_err());
        assert!(validate_domain("1.2.3.4", None).is_err());
        assert!(validate_domain("a_b.example.org", None).is_err());
        assert!(validate_domain("-a.example.org", None).is_err());
        assert!(validate_domain("x.cloud.example.com", Some("cloud.example.com")).is_err());
        assert!(validate_domain("cloud.example.com", Some("cloud.example.com")).is_err());
        assert!(validate_domain("evilcloud.example.com", Some("cloud.example.com")).is_ok());
    }

    fn domain(txt: &str) -> Domain {
        Domain {
            host: "shop.kunde.de".into(),
            project: "shop".into(),
            txt_value: txt.into(),
            status: "pending".into(),
            created_at: 0,
            verified_at: None,
            last_checked: None,
            last_error: None,
            cname_ok: false,
        }
    }

    #[tokio::test]
    async fn txt_must_match_and_cname_is_informational() {
        let dns = StaticDns::default();
        let d = domain("orbit-verify=abc");
        let c = check(&dns, Some("cloud.example.com"), &d).await;
        assert!(!c.verified && c.error.unwrap().contains("no TXT record"));
        dns.set_txt("_orbit-verify.shop.kunde.de", &["something else"]);
        assert!(!check(&dns, None, &d).await.verified);
        dns.set_txt("_orbit-verify.shop.kunde.de", &["\"orbit-verify=abc\""]);
        let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        dns.set_addrs("shop.kunde.de", &[ip]);
        dns.set_addrs("shop.cloud.example.com", &[ip]);
        let c = check(&dns, Some("cloud.example.com"), &d).await;
        assert!(c.verified && c.cname_ok && c.error.is_none());
    }
}
