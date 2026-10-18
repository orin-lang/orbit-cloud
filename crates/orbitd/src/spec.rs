//! The deployment spec: what an app is (job, service, consumer), what it may do (capabilities) and its limits. Every
//! capability is off unless the spec names it (Orbit ADR-0003, deny by default); `log/v1`, `clock/v1` and
//! `random/v1` are on by default because every run needs them for observability and they reach nothing outside.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use orbit_abi::CapabilityId;
use orbit_caps::{FsGrant, MountMode, NetScope, ProcessGrant, net_grant_config};
use orbit_runtime::{Grants, Limits};
use serde::{Deserialize, Serialize};

/// What kind of app a deployment is.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Runs on demand (`POST /v1/apps/{app}/runs`) and ends.
    #[default]
    Job,
    /// Runs as long as it is deployed; restarted by its policy.
    Service,
    /// Runs once per message of its queue, with the message on stdin.
    Consumer,
}

/// When a service is started again after it ended.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// After every end.
    #[default]
    Always,
    /// After a failed run only.
    OnFailure,
    /// Never.
    Never,
}

/// Resource limits of one run.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SpecLimits {
    /// Linear memory, MiB (1..=4096).
    pub memory_mb: u64,
    /// Wall-clock limit of a run in milliseconds; 0 = none (default for services: none, for others: 60 s).
    pub timeout_ms: Option<u64>,
    /// Fuel per run (deterministic CPU budget); none = unmetered.
    pub fuel: Option<u64>,
}

impl Default for SpecLimits {
    fn default() -> Self {
        Self {
            memory_mb: 64,
            timeout_ms: None,
            fuel: None,
        }
    }
}

/// The deployment spec, as posted with a module.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Spec {
    /// Job, service or consumer.
    pub kind: Kind,
    /// Export to call. Default: `_start` of a command module, else `orin_main` of an Orin program.
    pub entry: Option<String>,
    /// Program arguments after `argv[0]` (which is the app name), for `orbit:process/v1` and WASI.
    pub args: Vec<String>,
    /// Environment.
    pub env: BTreeMap<String, String>,
    /// KV namespaces of `orbit:kv/v1` the app may use; empty = no KV.
    pub kv_namespaces: Vec<String>,
    /// `ADDR:PORT` or `ADDR:LO-HI` the app may listen on (`orbit:net/v1`).
    pub listen: Vec<String>,
    /// `HOST:PORT` patterns the app may connect to and resolve (`orbit:net/v1`).
    pub connect: Vec<String>,
    /// A persistent read-write directory of the app, mounted at `/data` (`orbit:fs/v1`).
    pub files: bool,
    /// `orbit:clock/v1` (default on).
    pub clock: Option<bool>,
    /// `orbit:random/v1` (default on).
    pub random: Option<bool>,
    /// Limits.
    pub limits: SpecLimits,
    /// Restart policy of a service.
    pub restart: Restart,
    /// The queue a consumer reads.
    pub queue: Option<String>,
    /// Deliveries of a message before it is dead-lettered (consumer; default 5).
    pub max_attempts: Option<i64>,
}

impl Spec {
    /// Checks the spec; the error text is for the API caller.
    ///
    /// # Errors
    /// A description of the first problem.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=4096).contains(&self.limits.memory_mb) {
            return Err("limits.memory_mb must be 1..=4096".into());
        }
        for s in self.listen.iter().chain(&self.connect) {
            NetScope::parse(s).map_err(|e| format!("bad network scope `{s}`: {e}"))?;
        }
        for ns in &self.kv_namespaces {
            if ns.is_empty() || ns.len() > 64 {
                return Err(format!("bad kv namespace `{ns}`"));
            }
        }
        if self.env.keys().any(|k| k.is_empty() || k.contains('=')) {
            return Err("env names must be non-empty and contain no `=`".into());
        }
        match (self.kind, &self.queue) {
            (Kind::Consumer, None) => return Err("a consumer needs `queue`".into()),
            (Kind::Consumer, Some(q)) if !oc_store::valid_name(q) => {
                return Err(format!("bad queue name `{q}`"));
            }
            (Kind::Job | Kind::Service, Some(_)) => return Err("`queue` is for consumers".into()),
            _ => {}
        }
        if matches!(self.max_attempts, Some(n) if !(1..=100).contains(&n)) {
            return Err("max_attempts must be 1..=100".into());
        }
        Ok(())
    }

    /// The Orbit limits of one run.
    #[must_use]
    pub fn limits(&self) -> Limits {
        let mut l = Limits {
            max_memory_bytes: self.limits.memory_mb * 1024 * 1024,
            ..Limits::default()
        };
        l.fuel = self.limits.fuel;
        let default_ms = if self.kind == Kind::Service {
            0
        } else {
            60_000
        };
        let ms = self.limits.timeout_ms.unwrap_or(default_ms);
        l.timeout = (ms > 0).then(|| Duration::from_millis(ms));
        l
    }

    /// The capability grants of a run. `stdin_key` names the stdin bytes of this run (see `executor::RunStdin`);
    /// `files_dir` is the app's persistent directory when `files` is on.
    ///
    /// # Errors
    /// When the files directory cannot be described as a grant.
    pub fn grants(
        &self,
        app: &str,
        stdin_key: Option<&str>,
        files_dir: Option<&Path>,
    ) -> Result<Grants, String> {
        let mut g = Grants::none().grant(CapabilityId::log_v1());
        if self.clock != Some(false) {
            g = g.grant(CapabilityId::clock_v1());
        }
        if self.random != Some(false) {
            g = g.grant(CapabilityId::random_v1());
        }
        if !self.kv_namespaces.is_empty() {
            g = g.grant_with(
                CapabilityId::kv_v1(),
                serde_json::json!({ "namespaces": self.kv_namespaces }),
            );
        }
        if !self.listen.is_empty() || !self.connect.is_empty() {
            g = g.grant_with(
                CapabilityId::net_v1(),
                net_grant_config(&self.listen, &self.connect),
            );
        }
        if let (true, Some(dir)) = (self.files, files_dir) {
            let fg = FsGrant::new()
                .mount("/data", dir, MountMode::ReadWrite)
                .cwd("/data");
            g = g.grant_with(CapabilityId::fs_v1(), fg.to_config()?);
        }
        let argv: Vec<String> = std::iter::once(app.to_owned())
            .chain(self.args.iter().cloned())
            .collect();
        let mut p = ProcessGrant::new()
            .args(argv.clone())
            .stdin(stdin_key.is_some());
        for (k, v) in &self.env {
            p = p.env(k.as_str(), v.as_str());
        }
        let mut pc = p.to_config();
        if let (Some(key), Some(o)) = (stdin_key, pc.as_object_mut()) {
            o.insert(crate::executor::STDIN_KEY.into(), key.into());
        }
        g = g.grant_with(CapabilityId::process_v1(), pc);
        g = g.grant_with(
            CapabilityId::wasi_p1_v1(),
            serde_json::json!({ "args": argv, "env": self.env }),
        );
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let s: Spec = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(s.kind, Kind::Job);
        s.validate().unwrap();
        assert_eq!(s.limits().timeout, Some(Duration::from_secs(60)));
        let svc: Spec = serde_json::from_value(
            serde_json::json!({"kind": "service", "listen": ["0.0.0.0:8080"]}),
        )
        .unwrap();
        svc.validate().unwrap();
        assert_eq!(
            svc.limits().timeout,
            None,
            "a service runs without a wall-clock limit"
        );
        assert!(serde_json::from_value::<Spec>(serde_json::json!({"bogus": 1})).is_err());
        let c: Spec = serde_json::from_value(serde_json::json!({"kind": "consumer"})).unwrap();
        assert!(c.validate().is_err());
        let bad: Spec =
            serde_json::from_value(serde_json::json!({"listen": ["nonsense"]})).unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn grants_are_deny_by_default() {
        let s = Spec::default();
        let g = s.grants("app", None, None).unwrap();
        assert!(g.contains(&CapabilityId::log_v1()));
        assert!(!g.contains(&CapabilityId::kv_v1()));
        assert!(!g.contains(&CapabilityId::net_v1()));
        assert!(!g.contains(&CapabilityId::fs_v1()));
        let s = Spec {
            clock: Some(false),
            kv_namespaces: vec!["a".into()],
            ..Spec::default()
        };
        let g = s.grants("app", Some("k"), None).unwrap();
        assert!(!g.contains(&CapabilityId::clock_v1()));
        assert!(g.contains(&CapabilityId::kv_v1()));
        let pc = g.config(&CapabilityId::process_v1()).unwrap();
        assert_eq!(pc["stdin"], true);
        assert_eq!(pc[crate::executor::STDIN_KEY], "k");
    }
}
