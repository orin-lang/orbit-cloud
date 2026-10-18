//! The node: the executor plus one worker per service or consumer app. `reconcile(app)` makes the worker match the
//! app's active deployment (start, replace, stop); it is called at boot and after every deploy, rollback or delete.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oc_store::{Deployment, NackOutcome, RunOutcome, Store};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::executor::{Executor, RunRequest};
use crate::spec::{Kind, Restart, Spec};

const DEFAULT_MAX_ATTEMPTS: i64 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

struct Worker {
    deployment: i64,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// One Orbit Cloud node.
pub struct Node {
    /// Runs deployments.
    pub executor: Arc<Executor>,
    workers: tokio::sync::Mutex<HashMap<String, Worker>>,
    /// Woken on every enqueue so idle consumers poll at once.
    pub queue_signal: Arc<Notify>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node").finish_non_exhaustive()
    }
}

impl Node {
    /// A node around `executor`.
    #[must_use]
    pub fn new(executor: Arc<Executor>) -> Arc<Self> {
        Arc::new(Self {
            executor,
            workers: tokio::sync::Mutex::new(HashMap::new()),
            queue_signal: Arc::new(Notify::new()),
        })
    }

    fn store(&self) -> &Arc<Store> {
        self.executor.store()
    }

    /// Starts the workers of every app (boot).
    ///
    /// # Errors
    /// Store failures.
    pub async fn start_all(self: &Arc<Self>) -> anyhow::Result<()> {
        for app in self.store().apps()? {
            self.reconcile(&app.name).await?;
        }
        Ok(())
    }

    /// Apps with a running worker and the deployment it runs.
    pub async fn workers(&self) -> Vec<(String, i64)> {
        let w = self.workers.lock().await;
        let mut v: Vec<_> = w
            .iter()
            .filter(|(_, w)| !w.task.is_finished())
            .map(|(k, w)| (k.clone(), w.deployment))
            .collect();
        v.sort();
        v
    }

    /// Makes the worker of `app` match its active deployment.
    ///
    /// # Errors
    /// Store failures.
    pub async fn reconcile(self: &Arc<Self>, app: &str) -> anyhow::Result<()> {
        let active = match self.store().app(app) {
            Ok(a) => a.active_deployment,
            Err(oc_store::StoreError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let deployment = match active {
            Some(id) => Some(self.store().deployment(id)?),
            None => None,
        };
        let spec: Option<Spec> = deployment
            .as_ref()
            .and_then(|d| serde_json::from_value(d.spec.clone()).ok());
        let wants_worker = matches!(
            spec.as_ref().map(|s| s.kind),
            Some(Kind::Service | Kind::Consumer)
        );
        let mut workers = self.workers.lock().await;
        if let Some(w) = workers.get(app)
            && wants_worker
            && Some(w.deployment) == active
            && !w.task.is_finished()
        {
            return Ok(()); // already running the right deployment
        }
        if let Some(old) = workers.remove(app) {
            let _ = old.stop.send(true);
            // The worker records its interrupted run as failed and ends; wait so ports are free for the new one.
            if tokio::time::timeout(Duration::from_secs(10), old.task)
                .await
                .is_err()
            {
                tracing::warn!("worker of `{app}` did not stop within 10 s");
            }
        }
        let (Some(d), Some(spec), true) = (deployment, spec, wants_worker) else {
            return Ok(());
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let node = Arc::clone(self);
        let id = d.id;
        let task = match spec.kind {
            Kind::Service => tokio::spawn(async move { node.service_loop(d, spec, stop_rx).await }),
            Kind::Consumer => {
                tokio::spawn(async move { node.consumer_loop(d, spec, stop_rx).await })
            }
            Kind::Job => return Ok(()),
        };
        workers.insert(
            app.to_owned(),
            Worker {
                deployment: id,
                stop: stop_tx,
                task,
            },
        );
        Ok(())
    }

    /// Stops every worker (shutdown).
    pub async fn stop_all(&self) {
        let mut workers = self.workers.lock().await;
        for (_, w) in workers.drain() {
            let _ = w.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(10), w.task).await;
        }
    }

    /// Runs `d` once; when `stop` fires first, the run is recorded as failed and `None` is returned.
    async fn run_until_stopped(
        &self,
        d: &Deployment,
        req: RunRequest,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<oc_store::Run> {
        let run = match self
            .store()
            .start_run(&d.app, d.id, &req.trigger, req.message_id)
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("cannot record a run of `{}`: {e}", d.app);
                return None;
            }
        };
        let id = run.id.clone();
        tokio::select! {
            r = self.executor.execute(d, run, req) => match r {
                Ok(run) => Some(run),
                Err(e) => {
                    tracing::error!("run {id} of `{}`: {e}", d.app);
                    None
                }
            },
            _ = stop.wait_for(|s| *s) => {
                let o = RunOutcome { status: "failed".into(), error: Some("stopped: the app was redeployed or deleted".into()), ..RunOutcome::default() };
                let _ = self.store().finish_run(&id, &o);
                None
            }
        }
    }

    async fn service_loop(
        self: Arc<Self>,
        d: Deployment,
        spec: Spec,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut failures: u32 = 0;
        loop {
            if *stop.borrow() {
                return;
            }
            let req = RunRequest {
                trigger: "service".into(),
                ..RunRequest::default()
            };
            let Some(run) = self.run_until_stopped(&d, req, &mut stop).await else {
                return;
            };
            let ok = run.status == "succeeded";
            failures = if ok { 0 } else { failures.saturating_add(1) };
            let again = match spec.restart {
                Restart::Always => true,
                Restart::OnFailure => !ok,
                Restart::Never => false,
            };
            if !again {
                tracing::info!(
                    "service `{}` ended ({}), restart policy says stop",
                    d.app,
                    run.status
                );
                return;
            }
            let delay = backoff(failures);
            tracing::info!(
                "service `{}` ended ({}); restarting in {:?}",
                d.app,
                run.status,
                delay
            );
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = stop.wait_for(|s| *s) => return,
            }
        }
    }

    async fn consumer_loop(
        self: Arc<Self>,
        d: Deployment,
        spec: Spec,
        mut stop: watch::Receiver<bool>,
    ) {
        let Some(queue) = spec.queue.clone() else {
            return;
        };
        let max_attempts = spec.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
        // The lease outlives the run's own time limit, so a message is not redelivered while its run is active.
        let lease_ms = i64::try_from(
            spec.limits()
                .timeout
                .unwrap_or(Duration::from_secs(600))
                .as_millis(),
        )
        .unwrap_or(i64::MAX)
        .saturating_add(30_000);
        loop {
            if *stop.borrow() {
                return;
            }
            let msg = match self.store().lease(&queue, lease_ms) {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!("consumer `{}`: lease failed: {e}", d.app);
                    None
                }
            };
            let Some(msg) = msg else {
                tokio::select! {
                    () = self.queue_signal.notified() => {}
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stop.wait_for(|s| *s) => return,
                }
                continue;
            };
            let req = RunRequest {
                trigger: "queue".into(),
                stdin: Some(msg.body.clone()),
                message_id: Some(msg.id),
                ..RunRequest::default()
            };
            let Some(run) = self.run_until_stopped(&d, req, &mut stop).await else {
                // Stopped mid-run: give the message back at once.
                let _ = self.store().nack(
                    msg.id,
                    &msg.lease_token,
                    "consumer stopped",
                    0,
                    max_attempts.max(msg.attempts + 1),
                );
                return;
            };
            let res = if run.status == "succeeded" {
                self.store().ack(msg.id, &msg.lease_token).map(|()| None)
            } else {
                let err = run.error.clone().unwrap_or_else(|| "failed".into());
                let delay = i64::try_from(
                    backoff(u32::try_from(msg.attempts).unwrap_or(u32::MAX)).as_millis(),
                )
                .unwrap_or(0);
                self.store()
                    .nack(msg.id, &msg.lease_token, &err, delay, max_attempts)
                    .map(Some)
            };
            match res {
                Ok(Some(NackOutcome::Dead)) => tracing::warn!(
                    "message {} of `{queue}` is dead after {} attempts",
                    msg.id,
                    msg.attempts
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("message {} of `{queue}`: {e}", msg.id),
            }
        }
    }
}

/// 0 failures: 1 s; then doubling up to a minute.
fn backoff(failures: u32) -> Duration {
    let secs = 1u64
        .checked_shl(failures.saturating_sub(1).min(16))
        .unwrap_or(u64::MAX);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(40), MAX_BACKOFF);
    }
}
