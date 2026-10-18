//! `GET /metrics`: Prometheus text exposition (format 0.0.4). Counters of this process plus totals from the store.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::api::AppState;

fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Renders the metrics.
#[must_use]
pub fn render(state: &AppState) -> String {
    let c = &state.node.executor.counters;
    let store = state.node.executor.store();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_runs Runs recorded, by app and status.\n# TYPE orbit_cloud_runs gauge"
    );
    for (app, status, n) in store.run_counts().unwrap_or_default() {
        let _ = writeln!(
            out,
            "orbit_cloud_runs{{app=\"{}\",status=\"{}\"}} {n}",
            label(&app),
            label(&status)
        );
    }
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_runs_started_total Runs started by this process.\n# TYPE orbit_cloud_runs_started_total counter"
    );
    let _ = writeln!(
        out,
        "orbit_cloud_runs_started_total {}",
        c.runs_started.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_runs_active Runs executing now.\n# TYPE orbit_cloud_runs_active gauge"
    );
    let _ = writeln!(
        out,
        "orbit_cloud_runs_active {}",
        c.runs_active.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_run_seconds_total Wall time of finished runs.\n# TYPE orbit_cloud_run_seconds_total counter"
    );
    #[allow(clippy::cast_precision_loss)]
    let secs = c.run_ms_total.load(Ordering::Relaxed) as f64 / 1000.0;
    let _ = writeln!(out, "orbit_cloud_run_seconds_total {secs}");
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_log_lines_total Log lines accepted by this process.\n# TYPE orbit_cloud_log_lines_total counter"
    );
    let _ = writeln!(
        out,
        "orbit_cloud_log_lines_total {}",
        c.log_lines.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_log_lines_stored Log lines in the store.\n# TYPE orbit_cloud_log_lines_stored gauge"
    );
    let _ = writeln!(
        out,
        "orbit_cloud_log_lines_stored {}",
        store.log_count().unwrap_or(0)
    );
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_queue_messages Queue messages by state.\n# TYPE orbit_cloud_queue_messages gauge"
    );
    for q in store.queues().unwrap_or_default() {
        for (state, n) in [
            ("ready", q.ready),
            ("leased", q.leased),
            ("dead", q.dead),
            ("done", q.done),
        ] {
            let _ = writeln!(
                out,
                "orbit_cloud_queue_messages{{queue=\"{}\",state=\"{state}\"}} {n}",
                label(&q.queue)
            );
        }
    }
    let _ = writeln!(
        out,
        "# HELP orbit_cloud_apps Apps.\n# TYPE orbit_cloud_apps gauge"
    );
    let _ = writeln!(
        out,
        "orbit_cloud_apps {}",
        store.apps().map(|a| a.len()).unwrap_or(0)
    );
    out
}

/// The handler.
pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut body = render(&state);
    let workers = state.node.workers().await;
    let _ = writeln!(
        body,
        "# HELP orbit_cloud_workers_up Service and consumer workers running.\n# TYPE orbit_cloud_workers_up gauge"
    );
    let _ = writeln!(body, "orbit_cloud_workers_up {}", workers.len());
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}
