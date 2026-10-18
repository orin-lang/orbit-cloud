//! Access records of proxied requests: one log line in the app's logs and one `http.request` span in the trace of
//! the app's current service run, written in batches like guest logs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use oc_store::{NewLog, Span, Store, random_id};
use tokio::sync::mpsc;

/// One proxied request.
#[derive(Clone, Debug)]
pub struct Access {
    /// App key.
    pub app: String,
    /// Host the client asked for.
    pub host: String,
    /// Method.
    pub method: String,
    /// Path (no query).
    pub path: String,
    /// Response status.
    pub status: u16,
    /// Start, Unix microseconds.
    pub start_us: i64,
    /// End, Unix microseconds.
    pub end_us: i64,
    /// Response length when known.
    pub bytes: Option<u64>,
    /// Client address.
    pub ip: Option<IpAddr>,
    /// Request id.
    pub request_id: String,
}

impl Access {
    fn line(&self) -> String {
        let ms = (self.end_us - self.start_us) / 1000;
        format!(
            "{} {} {} {}ms host={} ip={} id={}",
            self.method,
            self.path,
            self.status,
            ms,
            self.host,
            self.ip.map_or_else(|| "-".into(), |i| i.to_string()),
            self.request_id
        )
    }
}

/// Starts the writer; send records on the returned channel.
#[must_use]
pub fn spawn_writer(store: Arc<Store>) -> mpsc::UnboundedSender<Access> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Access>();
    tokio::spawn(async move {
        loop {
            let Some(first) = rx.recv().await else { break };
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            while batch.len() < 500 {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(a)) => batch.push(a),
                    _ => break,
                }
            }
            let s = Arc::clone(&store);
            let _ = tokio::task::spawn_blocking(move || write_batch(&s, &batch)).await;
        }
    });
    tx
}

fn write_batch(store: &Store, batch: &[Access]) {
    let mut runs: HashMap<&str, Option<(String, String)>> = HashMap::new();
    let mut logs = Vec::new();
    let mut spans = Vec::new();
    for a in batch {
        let run = runs
            .entry(a.app.as_str())
            .or_insert_with(|| store.running_run(&a.app).ok().flatten());
        logs.push(NewLog {
            ts: oc_store::now_ms(),
            app: a.app.clone(),
            run_id: run.as_ref().map(|r| r.0.clone()),
            level: if a.status >= 500 { "warn" } else { "info" }.into(),
            message: a.line(),
        });
        if let Some((run_id, trace_id)) = run {
            spans.push(Span {
                trace_id: trace_id.clone(),
                span_id: random_id(8),
                parent_id: None,
                run_id: run_id.clone(),
                name: "http.request".into(),
                start_us: a.start_us,
                end_us: a.end_us,
                attrs: serde_json::json!({
                    "method": a.method, "path": a.path, "host": a.host, "status": a.status,
                    "bytes": a.bytes, "request_id": a.request_id, "client_ip": a.ip.map(|i| i.to_string()),
                }),
            });
        }
    }
    if let Err(e) = store.append_logs(&logs) {
        tracing::warn!("access log: {e}");
    }
    if let Err(e) = store.insert_spans(&spans) {
        tracing::warn!("access spans: {e}");
    }
}
