//! Certificates for the HTTPS listener: stored ACME certificates, an operator-provided wildcard, and on-demand
//! issuance, for **known hosts only**.
//!
//! The gate is the type system: [`TlsManager::config_for`] takes a [`KnownHost`], and a `KnownHost` can only be made by
//! [`KnownHost::check`] against the routing tables (console host, project hosts, verified custom domains). A TLS
//! server name that is none of these never reaches the certificate authority.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use oc_store::Store;
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;

use super::dns::BoxFut;
use super::hosts::{HostKind, Routing};

/// Let's Encrypt production.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging.
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// A host the platform serves: the only kind of name a certificate is ever prepared for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHost(String);

impl KnownHost {
    /// `Some` when `host` is the console, a project host or a verified custom domain in `routing`.
    #[must_use]
    pub fn check(routing: &Routing, base: &str, host: &str) -> Option<Self> {
        match routing.classify(host, base) {
            HostKind::Unknown => None,
            _ => Some(Self(host.to_owned())),
        }
    }

    /// The host name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A certificate as issued.
#[derive(Clone, Debug)]
pub struct IssuedCert {
    /// Chain, PEM.
    pub cert_pem: String,
    /// Private key, PEM.
    pub key_pem: String,
}

/// Something that can obtain a certificate for a host (ACME in production, a fake in tests).
pub trait CertIssuer: Send + Sync {
    /// Orders a certificate for `host`.
    fn issue(&self, host: &str) -> BoxFut<Result<IssuedCert, String>>;
}

/// Responses to pending HTTP-01 challenges: token to key authorisation.
#[derive(Debug, Default)]
pub struct Challenges(Mutex<HashMap<String, String>>);

impl Challenges {
    /// The key authorisation for a token.
    #[must_use]
    pub fn get(&self, token: &str) -> Option<String> {
        self.map().get(token).cloned()
    }

    /// Publishes a response.
    pub fn insert(&self, token: &str, key_auth: &str) {
        self.map().insert(token.to_owned(), key_auth.to_owned());
    }

    /// Withdraws a response.
    pub fn remove(&self, token: &str) {
        self.map().remove(token);
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server config for a PEM chain and key, and the chain's expiry in Unix ms.
fn server_config(
    cert_pem: &str,
    key_pem: &str,
) -> Result<(Arc<ServerConfig>, i64, Vec<String>), String> {
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("bad certificate PEM: {e}"))?;
    let leaf = chain
        .first()
        .ok_or("the certificate file has no certificate")?;
    let (not_after, names) = {
        let (_, x) = x509_parser::parse_x509_certificate(leaf)
            .map_err(|e| format!("bad certificate: {e}"))?;
        let names = x
            .subject_alternative_name()
            .ok()
            .flatten()
            .map(|san| {
                san.value
                    .general_names
                    .iter()
                    .filter_map(|g| match g {
                        x509_parser::extensions::GeneralName::DNSName(n) => {
                            Some(n.to_ascii_lowercase())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        (x.validity().not_after.timestamp() * 1000, names)
    };
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| format!("bad key PEM: {e}"))?
        .ok_or("no private key in the key file")?;
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("certificate and key do not fit: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((Arc::new(cfg), not_after, names))
}

/// The operator's own certificate (usually `*.base` plus `base`).
pub struct Wildcard {
    config: Arc<ServerConfig>,
    names: Vec<String>,
}

impl Wildcard {
    /// Loads a certificate chain and key from PEM files.
    ///
    /// # Errors
    /// Unreadable or inconsistent files.
    pub fn load(cert: &Path, key: &Path) -> Result<Self, String> {
        let c = std::fs::read_to_string(cert).map_err(|e| format!("{}: {e}", cert.display()))?;
        let k = std::fs::read_to_string(key).map_err(|e| format!("{}: {e}", key.display()))?;
        Self::from_pem(&c, &k)
    }

    /// From PEM text.
    ///
    /// # Errors
    /// Inconsistent input.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, String> {
        let (config, _, names) = server_config(cert_pem, key_pem)?;
        Ok(Self { config, names })
    }

    /// Whether one of the certificate's names covers `host` (`*.x` covers exactly one extra label).
    #[must_use]
    pub fn covers(&self, host: &str) -> bool {
        self.names.iter().any(|n| match n.strip_prefix("*.") {
            Some(rest) => host
                .split_once('.')
                .is_some_and(|(label, tail)| !label.is_empty() && tail == rest),
            None => n == host,
        })
    }
}

struct Cached {
    config: Arc<ServerConfig>,
    not_after_ms: i64,
}

struct Backoff {
    failures: u32,
    retry_at: Instant,
}

/// Chooses and obtains the certificate for each host.
pub struct TlsManager {
    store: Arc<Store>,
    wildcard: Option<Wildcard>,
    issuer: Option<Arc<dyn CertIssuer>>,
    cache: RwLock<HashMap<String, Cached>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    backoff: Mutex<HashMap<String, Backoff>>,
    /// How long a handshake waits for an on-demand issuance.
    pub issue_wait: Duration,
}

impl std::fmt::Debug for TlsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsManager").finish_non_exhaustive()
    }
}

/// Delay after the `failures`-th failed order of a host: 1 min doubling to 6 h.
#[must_use]
pub fn backoff_delay(failures: u32) -> Duration {
    let mins = 1u64 << failures.saturating_sub(1).min(9);
    Duration::from_secs(mins * 60).min(Duration::from_secs(6 * 3600))
}

impl TlsManager {
    /// A manager. Without a wildcard and without an issuer no host gets a certificate except stored ones.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        wildcard: Option<Wildcard>,
        issuer: Option<Arc<dyn CertIssuer>>,
    ) -> Self {
        Self {
            store,
            wildcard,
            issuer,
            cache: RwLock::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            backoff: Mutex::new(HashMap::new()),
            issue_wait: Duration::from_secs(60),
        }
    }

    fn cached(&self, host: &str) -> Option<Arc<ServerConfig>> {
        let soon = oc_store::now_ms() + 60_000;
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            .filter(|c| c.not_after_ms > soon)
            .map(|c| Arc::clone(&c.config))
    }

    fn install(
        &self,
        host: &str,
        cert_pem: &str,
        key_pem: &str,
    ) -> Result<Arc<ServerConfig>, String> {
        let (config, not_after_ms, _) = server_config(cert_pem, key_pem)?;
        self.cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                host.to_owned(),
                Cached {
                    config: Arc::clone(&config),
                    not_after_ms,
                },
            );
        Ok(config)
    }

    fn stored_config(&self, host: &str) -> Option<Arc<ServerConfig>> {
        let row = self.store.cert(host).ok().flatten()?;
        if row.not_after < oc_store::now_ms() + 60_000 {
            return None;
        }
        self.install(host, &row.cert_pem, &row.key_pem).ok()
    }

    /// The TLS configuration for a known host: a stored certificate, else the operator wildcard, else (when an issuer
    /// is configured) a freshly ordered certificate.
    ///
    /// # Errors
    /// No certificate is available for the host (no issuer, a failed order, a back-off in force, or a timeout).
    pub async fn config_for(&self, host: &KnownHost) -> Result<Arc<ServerConfig>, String> {
        let h = host.as_str();
        if let Some(c) = self.cached(h).or_else(|| self.stored_config(h)) {
            return Ok(c);
        }
        if let Some(w) = self.wildcard.as_ref().filter(|w| w.covers(h)) {
            return Ok(Arc::clone(&w.config));
        }
        if self.issuer.is_none() {
            return Err(format!(
                "no certificate for {h} (no wildcard covers it and ACME is off)"
            ));
        }
        match tokio::time::timeout(self.issue_wait, self.obtain(h)).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "issuing a certificate for {h} is taking too long; the order continues in the background"
            )),
        }
    }

    /// Orders (or waits for another task's order of) the certificate of `host` and stores it.
    ///
    /// # Errors
    /// The order failed or is in back-off.
    pub async fn obtain(&self, host: &str) -> Result<Arc<ServerConfig>, String> {
        let Some(issuer) = self.issuer.clone() else {
            return Err("ACME is not configured".into());
        };
        let lock = Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(host.to_owned())
                .or_default(),
        );
        let _guard = lock.lock().await;
        if let Some(c) = self.cached(host).or_else(|| self.stored_config(host)) {
            return Ok(c); // another task finished while we waited
        }
        if let Some(b) = self
            .backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            && b.retry_at > Instant::now()
        {
            return Err(format!(
                "certificate order for {host} failed {} time(s); next attempt in {} s",
                b.failures,
                b.retry_at
                    .saturating_duration_since(Instant::now())
                    .as_secs()
            ));
        }
        // The order runs as its own task so that a handshake that gives up waiting does not cancel it.
        let h = host.to_owned();
        let order = tokio::spawn(async move { issuer.issue(&h).await });
        let result = order.await.map_err(|e| e.to_string()).and_then(|r| r);
        match result {
            Ok(cert) => {
                let (config, not_after, _) = server_config(&cert.cert_pem, &cert.key_pem)?;
                self.store
                    .put_cert(host, &cert.cert_pem, &cert.key_pem, not_after)
                    .map_err(|e| e.to_string())?;
                self.cache
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        host.to_owned(),
                        Cached {
                            config: Arc::clone(&config),
                            not_after_ms: not_after,
                        },
                    );
                self.backoff
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(host);
                tracing::info!("certificate for {host} obtained");
                Ok(config)
            }
            Err(e) => {
                let mut b = self
                    .backoff
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let failures = b.get(host).map_or(1, |x| x.failures + 1);
                b.insert(
                    host.to_owned(),
                    Backoff {
                        failures,
                        retry_at: Instant::now() + backoff_delay(failures),
                    },
                );
                tracing::warn!("certificate order for {host} failed ({failures}): {e}");
                Err(e)
            }
        }
    }

    /// Orders a fresh certificate for a host that has one stored (renewal).
    pub async fn renew(&self, host: &str) {
        self.cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(host);
        let Some(issuer) = self.issuer.clone() else {
            return;
        };
        let h = host.to_owned();
        match tokio::spawn(async move { issuer.issue(&h).await }).await {
            Ok(Ok(cert)) => {
                if let Ok((_, not_after, _)) = server_config(&cert.cert_pem, &cert.key_pem)
                    && self
                        .store
                        .put_cert(host, &cert.cert_pem, &cert.key_pem, not_after)
                        .is_ok()
                {
                    let _ = self.install(host, &cert.cert_pem, &cert.key_pem);
                    tracing::info!("certificate for {host} renewed");
                }
            }
            Ok(Err(e)) => tracing::warn!("renewing the certificate of {host}: {e}"),
            Err(e) => tracing::warn!("renewing the certificate of {host}: {e}"),
        }
    }

    /// Daily: renews stored certificates with less than 30 days left, for hosts `still_known` says are still served.
    pub async fn renew_loop(self: Arc<Self>, still_known: impl Fn(&str) -> bool + Send + 'static) {
        let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            let before = oc_store::now_ms() + 30 * 86_400_000;
            for host in self.store.certs_expiring(before).unwrap_or_default() {
                if still_known(&host) {
                    self.renew(&host).await;
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------ ACME

/// Orders certificates from an ACME CA with the HTTP-01 challenge. The challenge responses are served by the gateway
/// on port 80 from `challenges`.
pub struct AcmeIssuer {
    store: Arc<Store>,
    email: String,
    directory: String,
    ca_file: Option<std::path::PathBuf>,
    challenges: Arc<Challenges>,
    account: tokio::sync::OnceCell<Account>,
}

impl AcmeIssuer {
    /// An issuer for `directory`.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        email: String,
        directory: String,
        ca_file: Option<std::path::PathBuf>,
        challenges: Arc<Challenges>,
    ) -> Self {
        Self {
            store,
            email,
            directory,
            ca_file,
            challenges,
            account: tokio::sync::OnceCell::new(),
        }
    }

    fn builder(&self) -> Result<instant_acme::AccountBuilder, String> {
        match &self.ca_file {
            Some(p) => Account::builder_with_root(p),
            None => Account::builder(),
        }
        .map_err(|e| format!("ACME client: {e}"))
    }

    async fn account(&self) -> Result<&Account, String> {
        self.account
            .get_or_try_init(|| async {
                if let Some(json) = self
                    .store
                    .acme_account(&self.directory)
                    .map_err(|e| e.to_string())?
                {
                    let creds = serde_json::from_str(&json)
                        .map_err(|e| format!("stored ACME account: {e}"))?;
                    return self
                        .builder()?
                        .from_credentials(creds)
                        .await
                        .map_err(|e| format!("ACME account: {e}"));
                }
                let contact = format!("mailto:{}", self.email);
                let (account, creds) = self
                    .builder()?
                    .create(
                        &NewAccount {
                            contact: &[contact.as_str()],
                            terms_of_service_agreed: true,
                            only_return_existing: false,
                        },
                        self.directory.clone(),
                        None,
                    )
                    .await
                    .map_err(|e| format!("creating the ACME account: {e}"))?;
                let json = serde_json::to_string(&creds).map_err(|e| e.to_string())?;
                self.store
                    .put_acme_account(&self.directory, &json)
                    .map_err(|e| e.to_string())?;
                tracing::info!("ACME account created at {}", self.directory);
                Ok(account)
            })
            .await
    }

    async fn order(&self, host: &str) -> Result<IssuedCert, String> {
        let account = self.account().await?;
        let ids = [Identifier::Dns(host.to_owned())];
        let mut order = account
            .new_order(&NewOrder::new(&ids))
            .await
            .map_err(|e| format!("new order: {e}"))?;
        let mut tokens = Vec::new();
        {
            let mut authzs = order.authorizations();
            while let Some(res) = authzs.next().await {
                let mut authz = res.map_err(|e| format!("authorization: {e}"))?;
                match authz.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    s => return Err(format!("authorization is {s:?}")),
                }
                let mut ch = authz
                    .challenge(ChallengeType::Http01)
                    .ok_or("the CA offers no http-01 challenge")?;
                let token = ch.token.clone();
                self.challenges
                    .insert(&token, ch.key_authorization().as_str());
                tokens.push(token);
                ch.set_ready()
                    .await
                    .map_err(|e| format!("challenge: {e}"))?;
            }
        }
        let result = async {
            let status = order
                .poll_ready(&RetryPolicy::default())
                .await
                .map_err(|e| format!("waiting for validation: {e}"))?;
            if status != OrderStatus::Ready {
                return Err(format!(
                    "the order is {status:?}; validation of {host} failed (is port 80 reachable?)"
                ));
            }
            let key_pem = order
                .finalize()
                .await
                .map_err(|e| format!("finalize: {e}"))?;
            let cert_pem = order
                .poll_certificate(&RetryPolicy::default())
                .await
                .map_err(|e| format!("certificate: {e}"))?;
            Ok(IssuedCert { cert_pem, key_pem })
        }
        .await;
        for t in tokens {
            self.challenges.remove(&t);
        }
        result
    }
}

impl CertIssuer for Arc<AcmeIssuer> {
    fn issue(&self, host: &str) -> BoxFut<Result<IssuedCert, String>> {
        let me = Arc::clone(self);
        let host = host.to_owned();
        Box::pin(async move { me.order(&host).await })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use oc_store::GatewaySnapshot;

    use super::*;

    fn self_signed(names: &[&str]) -> (String, String) {
        let k = rcgen::generate_simple_self_signed(
            names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        (k.cert.pem(), k.signing_key.serialize_pem())
    }

    struct Fake {
        calls: AtomicUsize,
        fail: bool,
    }

    impl CertIssuer for Arc<Fake> {
        fn issue(&self, host: &str) -> BoxFut<Result<IssuedCert, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = self.fail;
            let (cert_pem, key_pem) = self_signed(&[host]);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if fail {
                    Err("CA says no".into())
                } else {
                    Ok(IssuedCert { cert_pem, key_pem })
                }
            })
        }
    }

    fn routing() -> Routing {
        Routing::from_snapshot(GatewaySnapshot {
            generation: 1,
            projects: vec!["shop".into()],
            domains: vec![("shop.kunde.de".into(), "shop".into())],
            ..GatewaySnapshot::default()
        })
    }

    #[test]
    fn only_known_hosts_become_known() {
        let r = routing();
        let base = "cloud.example.com";
        assert!(KnownHost::check(&r, base, "shop.cloud.example.com").is_some());
        assert!(KnownHost::check(&r, base, "shop.kunde.de").is_some());
        assert!(KnownHost::check(&r, base, "console.cloud.example.com").is_some());
        assert!(KnownHost::check(&r, base, "random.cloud.example.com").is_none());
        assert!(KnownHost::check(&r, base, "attacker.example.org").is_none());
    }

    #[test]
    fn wildcard_covers_one_label() {
        let (c, k) = self_signed(&["*.cloud.example.com", "cloud.example.com"]);
        let w = Wildcard::from_pem(&c, &k).unwrap();
        assert!(w.covers("shop.cloud.example.com"));
        assert!(w.covers("cloud.example.com"));
        assert!(!w.covers("a.b.cloud.example.com"));
        assert!(!w.covers("shop.kunde.de"));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(60));
        assert_eq!(backoff_delay(2), Duration::from_secs(120));
        assert_eq!(backoff_delay(30), Duration::from_secs(6 * 3600));
    }

    #[tokio::test]
    async fn on_demand_issue_is_stored_cached_and_deduplicated() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fake = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let m = Arc::new(TlsManager::new(
            Arc::clone(&store),
            None,
            Some(Arc::new(Arc::clone(&fake))),
        ));
        let host = KnownHost::check(&routing(), "cloud.example.com", "shop.kunde.de").unwrap();
        let (a, b) = tokio::join!(m.config_for(&host), m.config_for(&host));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "concurrent handshakes share one order"
        );
        assert!(
            store.cert("shop.kunde.de").unwrap().is_some(),
            "stored for the next start"
        );
        m.config_for(&host).await.unwrap();
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "served from the cache"
        );
        // a new manager (restart) finds it in the store
        let m2 = TlsManager::new(Arc::clone(&store), None, Some(Arc::new(Arc::clone(&fake))));
        m2.config_for(&host).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_orders_back_off() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fake = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            fail: true,
        });
        let m = TlsManager::new(store, None, Some(Arc::new(Arc::clone(&fake))));
        let host = KnownHost::check(&routing(), "cloud.example.com", "shop.kunde.de").unwrap();
        assert!(m.config_for(&host).await.is_err());
        let e = m.config_for(&host).await.unwrap_err();
        assert!(e.contains("next attempt"), "{e}");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "the CA is not asked again during the back-off"
        );
    }

    #[tokio::test]
    async fn wildcard_is_used_without_acme_and_unknown_needs_issuer() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (c, k) = self_signed(&["*.cloud.example.com"]);
        let m = TlsManager::new(store, Some(Wildcard::from_pem(&c, &k).unwrap()), None);
        let r = routing();
        let shop = KnownHost::check(&r, "cloud.example.com", "shop.cloud.example.com").unwrap();
        assert!(m.config_for(&shop).await.is_ok());
        let custom = KnownHost::check(&r, "cloud.example.com", "shop.kunde.de").unwrap();
        assert!(
            m.config_for(&custom).await.is_err(),
            "not covered, and no ACME"
        );
    }
}
