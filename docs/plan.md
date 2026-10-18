# Orbit Cloud: plan

Orbit Cloud is the platform around the [Orbit](https://github.com/orin-lang/orbit) WebAssembly runtime: you hand it a
`.wasm` module (an Orin program built with `--target=wasm32-orbit`, or any module that speaks the Orbit host ABI) and a
small deployment spec, and it runs it as a job, a long-running service or a queue consumer, with logs, traces,
metrics, a database and queues built in. It ships as one binary, `orbitd`, that runs as a Docker container or as the
only service of a minimal appliance OS image.

## Goals

1. **One binary, zero external services.** A single `orbitd` process with an embedded SQLite database (WAL mode) is a
   complete installation: control plane, executor, log store, trace store, key-value database and queues. Nothing to
   install next to it.
2. **Orbit's security model end to end.** An app gets exactly the capabilities its spec names (deny by default, Orbit
   ADR-0003). Every app is its own Orbit tenant; its KV data, logs and queues are keyed by it.
3. **Observability by default.** Every execution is a *run* with a trace (spans for load, instantiate, execute), its log
   lines and its resource numbers (wall time, fuel, peak memory, host calls). Prometheus metrics for the whole node.
4. **Two deployment shapes from the same binary:** an OCI image (`docker run`) and an appliance OS image (systemd +
   `orbitd`, read-only root, data on its own partition).
5. **Single node first, cluster later**, through the same API (Orbit roadmap milestone 6).

## Concepts

| Concept | What it is |
|---|---|
| **App** | A name (`[a-z0-9-]{1,63}`) and its deployments. The app name is the Orbit tenant. |
| **Deployment** | An immutable version of an app: the module (content-addressed by SHA-256, stored once) and a spec. Deploying makes it the active one; the old ones stay for rollback. |
| **Spec** | JSON: `kind` (`job`, `service`, `consumer`), entry point, args, env, the capabilities (`kv_namespaces`, `listen`, `connect`, `clock`, `random`), limits (memory, timeout, fuel), restart policy, consumed queue. |
| **Run** | One execution of a deployment: status (`running`, `succeeded`, `failed`), exit code, error, timings, resource numbers, trace id. |
| **Log line** | A message the guest wrote through `orbit:log/v1` (and WASI `fd_write`, which the shim routes there), tagged with app, deployment and run. |
| **Span** | A timed step of a run (`run`, `load`, `instantiate`, `execute`) with attributes; the spans of a run form its trace. |
| **KV** | The persistent backend of `orbit:kv/v1`, per app and namespace; browsable through the API. |
| **Queue** | A durable FIFO with at-least-once delivery, visibility timeouts, retries and a dead-letter state. A `consumer` app is run once per message, with the message on stdin. |

## Milestones

### M1: the single-node core (this repository's first PR)

* `orbitd` with the SQLite store, the executor on `orbit-runtime` / `orbit-caps`, and the three app kinds:
  * **job**: run on demand (`POST /v1/apps/{app}/runs`);
  * **service**: started on deploy and at boot, supervised, restarted with exponential backoff (`restart: always | on-failure | never`);
  * **consumer**: one run per message of its queue; exit 0 acks, anything else retries with backoff, `max_attempts` moves the message to the dead letters.
* Logs: captured through a `LogSink`, batched into the store, queryable (`app`, `run`, `level`, `since`) and live-tailed
  over Server-Sent Events.
* Traces: spans per run; `GET /v1/runs/{id}/trace`.
* Metrics: `GET /metrics` in the Prometheus text format (runs by app and status, run duration, log lines, queue depth,
  services up).
* Database: the persistent `KvStore` behind `orbit:kv/v1`; `GET/PUT/DELETE /v1/apps/{app}/kv/{ns}/{key}`.
* Queues: `POST /v1/queues/{q}/messages`, receive with lease, ack, nack; dead letters listed and re-driven.
* Auth: one admin bearer token (`ORBIT_CLOUD_TOKEN`) for everything under `/v1`; `/healthz` and `/metrics` are open.
* Web console at `/`: apps, deployments, runs, a run's trace and logs, queues.
* `occtl`: the command-line client (deploy, apps, runs, logs `-f`, kv, queues).
* Packaging: Dockerfile, docker-compose, systemd unit, the appliance OS recipe (mkosi), CI.

### M2: production hardening

* OpenTelemetry: OTLP/HTTP export of spans and logs to an external collector, and W3C `traceparent` propagated into
  runs started through the API.
* Retention: per-app limits for log lines, runs and dead letters, applied by a background compactor.
* Schedules: cron-style triggers for jobs.
* HTTP ingress: one listener that routes `Host` / path prefixes to service apps (today a service listens on its own
  granted port).
* Users and API tokens with scopes instead of the single admin token; audit log of control-plane changes.
* Backups: online SQLite backup to a file or object storage.

### M3: richer data services

* SQL database per app (SQLite file per app first, PostgreSQL adapter later) once Orbit has an `orbit:sql/v1`
  capability (Orbit roadmap milestone 5).
* Object storage capability and API.
* Queue capability for guests (`orbit:queue/v1`: publish from inside an app), batch consumers.
* Secrets: encrypted values injected as env at run time.

### M4: cluster

* Several `orbitd` nodes: one leader for the control plane (Raft or an external PostgreSQL), executors pull work, the
  artifact store is shared or replicated; uses Orbit's control-plane protocol (Orbit roadmap milestone 6).
* Placement by capacity (memory reservations, invocation slots), node health and draining.

## Control plane in Orin

The API and the console are planned to move into Orin: ordinary Orin programs built on the libraries `orin-web` and
`orin-ui`, run by `orbitd` itself as **system apps** on Orbit, deployed with the node and holding grants like any other
app. The Rust host stays minimal: the executor, the supervisor, the store and the bootstrap of the system apps, nothing
that is policy or presentation.

What they need from the host is one new capability that exposes the store to them, `orbit-cloud:admin/v1`: apps,
deployments, runs, logs, traces, KV browsing and queues, plus lifecycle operations (deploy, activate, delete, run). It
is granted only to system apps, never to user apps, and its design (the shape of the calls, what is a stream, how the
token or the users of milestone 2 map onto it) comes later, together with the first system app. Until then the Rust
API and console of milestone 1 stay as they are, and the `/v1` HTTP API is the compatibility surface the Orin version
has to keep.

The console is the first piece: [`console/`](../console/README.md) is an Orin program on `orin-web` and `orin-ui` that runs
natively next to `orbitd` and uses only the `/v1` API (no new host capability). Running it as a system app on Orbit is blocked
on `std:net`/`std:http` for `wasm32-orbit` and on request delivery to wasm apps.

## Deployment shapes

* **Docker**: `docker run -p 8080:8080 -v orbit-data:/var/lib/orbit-cloud -e ORBIT_CLOUD_TOKEN=... ghcr.io/orin-lang/orbit-cloud`.
  Ports that services listen on are published like any container port.
* **Appliance OS**: `deploy/os/` builds a minimal Debian image with [mkosi](https://github.com/systemd/mkosi): systemd,
  `orbitd` as the only service, read-only `/usr`, the data directory on its own partition, no shell login by default
  (serial console for recovery). It boots in a VM or on bare metal; updates are image swaps (A/B partitions, M2).

## Non-goals for M1

Multi-node operation, users and roles, TLS termination (put a reverse proxy in front), arbitrary containers (the unit of
deployment is a WebAssembly module, by design).
