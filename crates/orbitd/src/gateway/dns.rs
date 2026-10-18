//! DNS lookups for domain verification, behind a trait so tests can answer from a table.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use hickory_resolver::TokioResolver;

/// A boxed future.
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the verifier needs from DNS.
pub trait DnsLookup: Send + Sync + std::fmt::Debug {
    /// The TXT records of `name`, each as one string (character-strings joined).
    fn txt(&self, name: &str) -> BoxFut<Result<Vec<String>, String>>;
    /// The addresses `name` resolves to (following CNAMEs).
    fn addrs(&self, name: &str) -> BoxFut<Result<Vec<IpAddr>, String>>;
}

/// The system resolver (`/etc/resolv.conf`), created on first use.
#[derive(Default)]
pub struct SystemDns {
    resolver: OnceLock<Result<TokioResolver, String>>,
}

impl std::fmt::Debug for SystemDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemDns").finish_non_exhaustive()
    }
}

impl SystemDns {
    fn resolver(&self) -> Result<TokioResolver, String> {
        self.resolver
            .get_or_init(|| {
                TokioResolver::builder_tokio()
                    .and_then(hickory_resolver::ResolverBuilder::build)
                    .map_err(|e| format!("cannot set up the DNS resolver: {e}"))
            })
            .clone()
    }
}

impl DnsLookup for SystemDns {
    fn txt(&self, name: &str) -> BoxFut<Result<Vec<String>, String>> {
        let r = self.resolver();
        let name = format!("{}.", name.trim_end_matches('.'));
        Box::pin(async move {
            let r = r?;
            match r.txt_lookup(name.as_str()).await {
                Ok(l) => Ok(l
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        hickory_resolver::proto::rr::RData::TXT(t) => Some(
                            t.txt_data
                                .iter()
                                .map(|d| String::from_utf8_lossy(d).into_owned())
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect()),
                Err(e) if e.is_no_records_found() => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn addrs(&self, name: &str) -> BoxFut<Result<Vec<IpAddr>, String>> {
        let r = self.resolver();
        let name = format!("{}.", name.trim_end_matches('.'));
        Box::pin(async move {
            let r = r?;
            match r.lookup_ip(name.as_str()).await {
                Ok(l) => Ok(l.iter().collect()),
                Err(e) if e.is_no_records_found() => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

/// Answers from a table (tests, and `--dns-static` style setups).
#[derive(Debug, Default, Clone)]
pub struct StaticDns {
    txt: Arc<Mutex<HashMap<String, Vec<String>>>>,
    addrs: Arc<Mutex<HashMap<String, Vec<IpAddr>>>>,
}

impl StaticDns {
    /// Sets the TXT records of a name.
    pub fn set_txt(&self, name: &str, values: &[&str]) {
        self.txt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                name.to_owned(),
                values.iter().map(|s| (*s).to_owned()).collect(),
            );
    }

    /// Sets the addresses of a name.
    pub fn set_addrs(&self, name: &str, values: &[IpAddr]) {
        self.addrs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), values.to_vec());
    }
}

impl DnsLookup for StaticDns {
    fn txt(&self, name: &str) -> BoxFut<Result<Vec<String>, String>> {
        let v = self
            .txt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name.trim_end_matches('.'))
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(v) })
    }

    fn addrs(&self, name: &str) -> BoxFut<Result<Vec<IpAddr>, String>> {
        let v = self
            .addrs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name.trim_end_matches('.'))
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(v) })
    }
}
