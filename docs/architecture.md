# Architecture

Orbit Cloud is one process, `orbitd`, around one SQLite file. This page describes the parts, what happens during a
run, how traces are recorded, the schema, the security model and the limits of milestone 1. The plan and later
milestones are in [plan.md](plan.md); the HTTP reference is [api.md](api.md).

## Components

```
            occtl / curl / browser                       Prometheus
                     |                                       |
              +------v---------------------------------------v-----+
              |  orbitd                                            |
              |   HTTP API (axum)   /v1, /, /healthz, /metrics     |
              |        |                                           |
              |      Node  ---- supervisor: one worker task per    |
              |        |        service or consumer app            |
              |      Executor ---- Orbit runtime + capabilities    |
              |        |             (orbit-runtime, orbit-caps)   |
              |      oc-store ---- SQLite (WAL) in --data-dir      |
              +----------------------------------------------------+
                  data dir:  orbit-cloud.db   files/<app>/
```

| Component | Crate | Job |
|---|---|---|
| Store | `oc-store` | All persistence: apps, content-addressed modules, deployments, runs, logs, spans, KV, queues. One SQLite connection behind a mutex, WAL mode, foreign keys on. The only crate that knows SQL. |
| Executor | `orbitd::executor` | Runs one deployment once. Loads the module on `orbit-runtime` (compiled artifacts are cached by content hash), builds the capability grants from the spec, spawns an isolate, calls the entry point, records the run, its spans and its log lines. |
| Node (supervisor) | `orbitd::node` | `reconcile(app)` makes the worker of an app match its active deployment: start, replace or stop. Services get a restart loop with exponential backoff (1 s up to 60 s); consumers get a lease loop over their queue. Called at boot and after every deploy, rollback and delete. |
| API | `orbitd::api` | The HTTP surface, bearer-token authentication, the Server-Sent-Events log stream, the console page. Handlers hold no state of their own. |
| Metrics | `orbitd::metrics` | Prometheus text exposition from process counters and store totals. |
| Spec | `orbitd::spec` | The deployment spec: parsing, validation, translation to Orbit `Grants` and `Limits`. |
| `occtl` | `occtl` | A thin blocking HTTP client; every command is one or two API calls. |

The Orbit crates are pinned to one commit of `orin-lang/orbit` in the workspace `Cargo.toml`. The host ABI is versioned
per capability (`orbit:kv/v1`, ...), so bumping the pin is a deliberate act.

## The three kinds of app

* **job**: runs on demand through `POST /v1/apps/{app}/runs`. The request body becomes the run's stdin; query
  parameters give integer arguments and `detach`.
* **service**: started when deployed and at boot; when it ends, the restart policy (`always`, `on-failure`, `never`)
  decides, with backoff that resets after a successful run. It is stopped (its run recorded as `failed`, "stopped: the
  app was redeployed or deleted") when a new deployment replaces it. A service reaches the network only through its
  own `listen` grant.
* **consumer**: one run per message of its queue, the message on stdin. Exit 0 acknowledges. Any other outcome nacks
  with backoff until `max_attempts` (default 5), then the message is dead-lettered. The lease is the run's time limit
  plus 30 s, so a message is not redelivered while its run is alive; a crashed `orbitd` makes leases expire and the
  messages are delivered again (at-least-once).

## Run lifecycle

1. **Record.** `Store::start_run` inserts a `runs` row with status `running`, a random run id and a random trace id.
2. **Load** (span `load`). The module bytes come from the store by SHA-256; `Runtime::load` compiles them or returns
   the cached artifact.
3. **Instantiate** (span `instantiate`). The spec becomes Orbit `Grants` (below) and `Limits` (memory, wall-clock
   timeout, fuel). If the run has stdin, the bytes are parked in memory under a one-time key that the `process/v1`
   grant carries; the isolate takes them on creation. The isolate id is mapped to the run id so log lines can be
   tagged.
4. **Execute** (span `execute`). `init` runs the module's initialisation, then the entry point is invoked: the spec's
   `entry`, else `_start` of a command module, else `orin_main` of an Orin program (whose integer result is the exit
   status; a trap after `orin_exit_status` was set is read as that status).
5. **Finish.** Spans are inserted, the log writer is flushed so every line of the run is already queryable, and the
   run row is closed with status `succeeded` (exit 0) or `failed`, exit code, error text, wall time, fuel, peak memory
   and the host calls per capability.

A run that cannot be set up (bad module, instantiate error) is still a recorded `failed` run with the error and the
spans reached so far. At boot `orbitd` marks runs left `running` by a previous process as failed.

Log lines travel from the guest through the `LogSink` into an unbounded channel and are written to SQLite in batches
(up to 500 lines or 200 ms) by one task, so a chatty guest never waits for the disk.

## Trace model

A trace is the set of spans of one run; `trace_id` is on the run, and `GET /v1/runs/{id}/trace` returns them.

| Span | Parent | Attributes |
|---|---|---|
| `run` | none | app, deployment, version, module, trigger (`api`, `service`, `queue`), status, exit_code, error |
| `load` | `run` | module, bytes (or the error) |
| `instantiate` | `run` | instance id (or the error) |
| `execute` | `run` | entry, exit_code, error, fuel, peak_memory_bytes, host_calls |

Times are Unix microseconds (`start_us`, `end_us`). Span and trace ids are random hex; there is no W3C trace-context
propagation or export yet (milestone 2).

## Storage schema

`crates/oc-store/src/schema.sql`, version 1 (`meta.schema_version`). Times are Unix milliseconds in UTC.

| Table | Key | Content |
|---|---|---|
| `meta` | `key` | `schema_version` |
| `apps` | `name` | `created_at`, `active_deployment` |
| `modules` | `sha256` | the `.wasm` bytes, size; one row per distinct module, shared by deployments and apps |
| `deployments` | `id`; unique `(app, version)` | `app` (cascade delete), `version` (1, 2, ...), `module`, `spec` (JSON), `created_at` |
| `runs` | `id` | `app`, `deployment`, `trigger`, `status`, `exit_code`, `error`, `started_at`, `finished_at`, `wall_ms`, `fuel`, `peak_memory`, `host_calls` (JSON), `trace_id`, `message_id`; index `(app, started_at DESC)` |
| `logs` | `id` (autoincrement) | `ts`, `app`, `run_id`, `level`, `message`; indexes `(app, id)` and `(run_id, id)` |
| `spans` | `(trace_id, span_id)` | `parent_id`, `run_id`, `name`, `start_us`, `end_us`, `attrs` (JSON) |
| `kv` | `(tenant, ns, key)` | `value`, `updated_at`; `tenant` is the app name |
| `queue_messages` | `id` (autoincrement) | `queue`, `body`, `state` (`ready`, `leased`, `done`, `dead`), `attempts`, `visible_at`, `enqueued_at`, `lease_token`, `last_error`; index `(queue, state, visible_at, id)` |

Queue semantics: `lease` picks the oldest message that is `ready`, or `leased` with an expired `visible_at`, sets it
leased for `lease_ms`, bumps `attempts` and returns a fresh `lease_token`. `ack` and `nack` must present the current
token, otherwise the call is a conflict (the lease expired and the message went to someone else). `nack` returns
`retry` (visible again after `delay_ms`) or `dead` (when `attempts >= max_attempts`).

Deleting an app removes its deployments, runs, spans, logs and KV rows in one transaction, then its files directory.
Modules and queue messages are not tied to an app and stay.

## Security model

**Deny by default.** An isolate gets exactly the capabilities its spec names (Orbit ADR-0003). Three are on unless
switched off because they reach nothing outside the isolate: `orbit:log/v1`, `orbit:clock/v1`, `orbit:random/v1`.
Everything else needs a field in the spec:

| Capability | Granted by | Scope |
|---|---|---|
| `orbit:kv/v1` | `kv_namespaces` | only the named namespaces |
| `orbit:queue/v1` | `publish` | publish only to the named queues (receiving is the consumer app kind); queue names are node-wide |
| PostgreSQL | `postgres` | a database and login role of the app's own on the operator's server (`--postgres-url`); `DATABASE_URL` is injected and the server's `HOST:PORT` added to the connect grant ([postgres.md](postgres.md)) |
| `orbit:net/v1` | `listen`, `connect` | only `ADDR:PORT`/`ADDR:LO-HI` to listen on and `HOST:PORT` patterns to connect to or resolve |
| `orbit:fs/v1` | `files: true` | one read-write directory, `files/<app>/` of the data dir, mounted at `/data`; nothing else of the host |
| `orbit:process/v1`, `wasi` | always | `argv[0]` is the app name, then `args`; `env` is exactly the spec's `env`; stdin is the run's input. No host environment, no spawning. |

A module that imports a capability it was not granted fails at instantiation with a clear error; the e2e tests cover
the KV case.

**A tenant per app.** The app name is the Orbit tenant of every isolate it runs. Orbit keys KV data, log records and
other per-tenant state by it, and `oc-store` keys `kv` rows by `(tenant, ns, key)`, so one app cannot read another's
data even when both use the same namespace name. Log lines carry the tenant and run. Deleting an app deletes its data,
including its files directory, so a new app of the same name starts empty.

**Resource limits** bound every run: linear memory (`limits.memory_mb`, default 64), wall-clock time (60 s for jobs and
consumers, none for services), and optional fuel when `--fuel-metering` is on. Payloads are capped (below).

**Control plane.** `/v1` requires `Authorization: Bearer <ORBIT_CLOUD_TOKEN>` (compared in constant time); the
Server-Sent-Events stream also accepts `?token=` because browsers cannot set headers on `EventSource`. `/healthz` and
`/metrics` are open. Without a token the API is open and `orbitd` says so in the log; the container and the appliance
require one. The token is a single admin credential: whoever has it can run any module with any grant the spec asks
for, so treat it like root on the node. Put TLS in front of it.

**Process hardening.** The systemd unit and the appliance run `orbitd` as an unprivileged user with no capabilities, a
read-only file system except the data directory, a syscall filter and `NoNewPrivileges`. WebAssembly isolation is the
inner boundary; the sandbox is the outer one.

## Limits of milestone 1

| Limit | Value |
|---|---|
| Module size | 64 MiB |
| Queue message and run input | 1 MiB |
| Memory per run | 1 to 4096 MiB (default 64) |
| Wall-clock time per run | 60 s for jobs and consumers (`limits.timeout_ms`, 0 = none), none for services |
| Delivery attempts | 1 to 100 (default 5) |
| Lease via the API | 1 ms to 1 h (default 30 s) |
| Log lines per request | 1 to 5000 (default 200) |
| Names (apps, queues) | `[a-z0-9-]`, 1 to 63 characters, not starting with `-` |

Not yet: retention (logs, runs, spans and dead letters grow until the app is deleted; milestone 2), multiple nodes,
users and scopes, TLS, schedules, secrets, OpenTelemetry export. SQLite serialises all writers behind one connection;
that is ample for one node, and a reason the cluster design (milestone 4) moves the control plane elsewhere.

### Queue backends

Queue logic sits behind the `MessageQueue` trait (`orbitd::queues`): publish, lease/ack/nack, stats, dead letters and
redrive. The built-in backend is the SQLite store, so queues survive restarts and need nothing else. The API, the
consumer workers and the guests' `orbit:queue/v1` (`GuestQueues`) only use the trait, and
`Executor::with_queues` takes another implementation. NATS JetStream is a planned optional backend (not implemented):
it would implement the same trait, with `lease` as a pull-consumer fetch, `ack`/`nack` as ack/nak with delay, and the
dead letters as a stream the max-deliveries advisory feeds.
