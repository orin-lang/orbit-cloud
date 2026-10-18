# HTTP API

`orbitd` serves JSON over HTTP on `--listen` (default `0.0.0.0:8080`).

* `/v1/*` needs `Authorization: Bearer $ORBIT_CLOUD_TOKEN` (when the node has a token). The log stream also accepts
  `?token=...`.
* `/healthz`, `/metrics` and `/` (the console) are open.
* Errors are `{"error": "message"}` with the status: 400 bad input, 401 auth, 404 not found, 409 conflict (a stale
  lease, an app with no deployment, running a service by hand), 413 payload too large, 500 store failure.
* Times are Unix milliseconds, except span times, which are microseconds.
* Names of apps and queues: 1 to 63 of `a-z`, `0-9`, `-`, not starting with `-`.

The examples use `H='Authorization: Bearer secret'` and `B=http://127.0.0.1:8080`. `occtl` wraps all of this; each
section names the command.

## Spec format

The spec is JSON, sent in the `x-orbit-spec` header of a deployment. Every field is optional; unknown fields are
rejected. Capabilities are off unless named (see [architecture.md](architecture.md#security-model)).

| Field | Type | Default | Meaning |
|---|---|---|---|
| `kind` | `"job"`, `"service"`, `"consumer"` | `"job"` | how the app runs |
| `entry` | string | `_start`, else `orin_main` | export to call |
| `args` | string[] | `[]` | program arguments after `argv[0]` (the app name) |
| `env` | map string to string | `{}` | environment of the run; names non-empty, no `=` |
| `kv_namespaces` | string[] | `[]` | namespaces of `orbit:kv/v1` the app may use |
| `listen` | string[] | `[]` | `ADDR:PORT` or `ADDR:LO-HI` the app may listen on |
| `connect` | string[] | `[]` | `HOST:PORT` patterns the app may connect to and resolve |
| `files` | bool | `false` | a persistent read-write directory mounted at `/data` |
| `clock`, `random` | bool | `true` | switch `orbit:clock/v1` / `orbit:random/v1` off with `false` |
| `limits.memory_mb` | integer 1..4096 | `64` | linear memory |
| `limits.timeout_ms` | integer | 60000 (jobs, consumers), none (services) | wall-clock limit per run; 0 = none |
| `limits.fuel` | integer | none | deterministic CPU budget; needs `orbitd --fuel-metering` |
| `restart` | `"always"`, `"on-failure"`, `"never"` | `"always"` | restart policy of a service |
| `queue` | string | none | the queue of a consumer (required for `consumer`, refused otherwise) |
| `max_attempts` | integer 1..100 | `5` | deliveries before a message is dead-lettered (consumer) |

```json
{ "kind": "consumer", "queue": "orders", "max_attempts": 3, "kv_namespaces": ["seen"],
  "limits": { "memory_mb": 32, "timeout_ms": 5000 } }
```

## Node

### `GET /healthz`
Returns `ok` (200). Container health check: `orbitd --healthcheck`.

### `GET /v1/node`
```sh
curl -H "$H" $B/v1/node
# {"kinds":["job","service","consumer"],"orbit_abi":1,"version":"0.1.0","workers":[{"app":"hub","deployment":4}]}
```
`workers` are the services and consumers running now. CLI: `occtl status`.

## Apps and deployments

### `GET /v1/apps`
```sh
curl -H "$H" $B/v1/apps
# [{"active_deployment":1,"created_at":1792336579470,"kind":"job","name":"hello","version":1,"worker_running":false}]
```
CLI: `occtl apps`.

### `GET /v1/apps/{app}`
The app, all its deployments (newest last) and the deployment its worker runs, if any.
```json
{"app":{"name":"hello","created_at":1792336579470,"active_deployment":1},
 "deployments":[{"id":1,"app":"hello","version":1,"module":"0fbd...b83d","spec":{...},"created_at":1792336579470}],
 "worker_deployment":null}
```
CLI: `occtl app hello`.

### `POST /v1/apps/{app}/deployments`
Deploys the request body (a WebAssembly module, up to 64 MiB) as the next version and makes it active. The app is
created on first use. The spec is in the `x-orbit-spec` header. A service or consumer is (re)started at once; a replaced
worker is stopped first. Returns 201 with the deployment.
```sh
curl -XPOST -H "$H" -H 'x-orbit-spec: {"kv_namespaces":["cfg"]}' --data-binary @hello.wasm $B/v1/apps/hello/deployments
# {"app":"hello","id":1,"module":"0fbd...b83d","spec":{...},"version":1,"created_at":1792336579470}
```
400 for a body that is not a module (`\0asm`), a bad app name or an invalid spec. CLI: `occtl deploy hello hello.wasm
--kv cfg`.

### `GET /v1/apps/{app}/deployments`
The deployments of the app, as an array.

### `POST /v1/apps/{app}/deployments/{id}/activate`
Rollback or roll-forward: makes deployment `id` (the `id`, not the `version`) active and reconciles the worker.
Returns the app. 404 if the deployment is not the app's. CLI: `occtl rollback hello 1` (takes the version).

### `DELETE /v1/apps/{app}`
Stops its worker and deletes the app with its deployments, runs, spans, logs, KV data and files directory. 204. CLI:
`occtl delete hello`.

## Runs, logs and traces

### `POST /v1/apps/{app}/runs`
Runs the active deployment once; the request body (up to 1 MiB) is the run's stdin. Waits for the end and returns the
run (200). Query: `args=1,2` (integer arguments of the entry export), `detach=true` (return 202 with the `running`
record at once). 409 for a service (it runs by itself) and for an app without deployment.
```sh
curl -XPOST -H "$H" "$B/v1/apps/hello/runs"
```
```json
{"id":"a93b0009ca0dd345","app":"hello","deployment":1,"trigger":"api","status":"succeeded","exit_code":0,"error":null,
 "started_at":1792336579483,"finished_at":1792336580029,"wall_ms":0,"fuel":null,"peak_memory":1835008,
 "host_calls":{"log/v1":1},"trace_id":"80eed6fd379d723139e9c2a6fd26043c","message_id":null}
```
`status` is `running`, `succeeded` or `failed`; `trigger` is `api`, `service` or `queue`; `message_id` is set for
consumer runs. CLI: `occtl run hello [--input file] [--args 1,2] [--detach]`.

### `GET /v1/apps/{app}/runs?limit=50` and `GET /v1/runs?limit=50`
Runs, newest first. CLI: `occtl runs [app]`.

### `GET /v1/runs/{id}`
One run.

### `GET /v1/runs/{id}/trace`
```json
{"trace_id":"80ee...","spans":[
  {"trace_id":"80ee...","span_id":"87f7...","parent_id":null,"run_id":"a93b...","name":"run","start_us":1792336579483737,"end_us":1792336580029024,"attrs":{"status":"succeeded","exit_code":0}},
  {"name":"load","parent_id":"87f7...","attrs":{"bytes":102298,"module":"0fbd..."}},
  {"name":"instantiate","attrs":{"instance":"i-1"}},
  {"name":"execute","attrs":{"entry":null,"exit_code":0,"fuel":null,"peak_memory_bytes":1835008,"host_calls":{"log/v1":1}}}]}
```
(Abridged: every span has all the fields of the first.) CLI: `occtl trace <run>`.

### `GET /v1/runs/{id}/logs`
The log lines of one run; same query as `/v1/logs`.

### `GET /v1/logs`
Log lines, oldest first, the newest `limit` that match.

| Query | Meaning |
|---|---|
| `app`, `run` | exact match |
| `level` | `trace`, `debug`, `info`, `warn` or `error` |
| `after` | only lines with `id` greater than this (for polling) |
| `since` | only lines at or after this time (ms) |
| `contains` | substring of the message |
| `limit` | 1..5000, default 200 |

```sh
curl -H "$H" "$B/v1/logs?app=hello&level=info"
# [{"app":"hello","id":1,"level":"info","message":"hi","run_id":"a93b0009ca0dd345","ts":1792336580028}]
```
CLI: `occtl logs --app hello --level info --contains hi`.

### `GET /v1/logs/stream`
Server-Sent Events: every new line matching the same filters, as `data: {json}`, starting after the newest line (or
after `after`). Polls the store every 250 ms, keep-alive comments in between.
```sh
curl -N -H "$H" "$B/v1/logs/stream?app=hello"
# data: {"app":"hello","id":2,"level":"info","message":"hi","run_id":"...","ts":...}
```
CLI: `occtl logs -f`.

## KV

The data behind `orbit:kv/v1`; the tenant is the app. Keys and values are bytes; listings show them as UTF-8 (lossy).

| Method and path | Meaning |
|---|---|
| `GET /v1/apps/{app}/kv` | namespaces with entry counts: `[{"namespace":"cfg","entries":1}]` |
| `GET /v1/apps/{app}/kv/{ns}?prefix=&limit=100` | entries: `[{"key":"color","value":"blue","size":4,"updated_at":...}]` |
| `GET /v1/apps/{app}/kv/{ns}/{key}` | the raw value; 404 if absent |
| `PUT /v1/apps/{app}/kv/{ns}/{key}` | the body becomes the value; 204; the app must exist |
| `DELETE /v1/apps/{app}/kv/{ns}/{key}` | 204, or 404 if absent |

```sh
curl -XPUT -H "$H" -d blue $B/v1/apps/hello/kv/cfg/color   # 204
curl -H "$H" $B/v1/apps/hello/kv/cfg/color                  # blue
```
CLI: `occtl kv ls|get|put|rm`. Writing through the API is not limited by the app's `kv_namespaces` grant (that grant
limits the guest, not the operator).

## Queues

Queues exist when the first message arrives. Messages up to 1 MiB.

### `POST /v1/queues/{q}/messages`
The body is the message. 201 `{"id":1,"queue":"orders"}`. Wakes idle consumers. CLI: `occtl queue send orders '{"id":1}'`.

### `GET /v1/queues`
`[{"queue":"orders","ready":0,"leased":1,"dead":0,"done":0}]`. CLI: `occtl queue ls`.

### `POST /v1/queues/{q}/lease?lease_ms=30000`
For external consumers (consumer apps use the same store directly). Leases the oldest deliverable message for
`lease_ms` (1 to 3 600 000), or returns 204 when there is none.
```json
{"id":1,"queue":"orders","body":"{\"id\":1}","attempts":1,"enqueued_at":1792336580145,"lease_token":"d247...","last_error":null}
```

### `POST /v1/queues/{q}/messages/{id}/ack`
Body `{"token":"<lease_token>"}`. 204. 409 if the lease is no longer current.

### `POST /v1/queues/{q}/messages/{id}/nack`
Body `{"token": "...", "error": "why", "delay_ms": 0, "max_attempts": 5}` (all but `token` optional). Returns
`{"outcome":"retry"}` or `{"outcome":"dead"}`.

### `GET /v1/queues/{q}/dead?limit=100`
Dead letters, as messages with `last_error`. CLI: `occtl queue dead orders`.

### `POST /v1/queues/{q}/redrive`
Makes the dead letters ready again with a fresh attempt count: `{"redriven": 2}`. CLI: `occtl queue redrive orders`.

## Metrics

`GET /metrics`, Prometheus text format 0.0.4, open.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `orbit_cloud_runs` | gauge | `app`, `status` | runs in the store |
| `orbit_cloud_runs_started_total` | counter | | runs started by this process |
| `orbit_cloud_runs_active` | gauge | | runs executing now |
| `orbit_cloud_run_seconds_total` | counter | | wall time of finished runs |
| `orbit_cloud_log_lines_total` | counter | | log lines accepted by this process |
| `orbit_cloud_log_lines_stored` | gauge | | log lines in the store |
| `orbit_cloud_queue_messages` | gauge | `queue`, `state` | `ready`, `leased`, `dead`, `done` |
| `orbit_cloud_apps` | gauge | | apps |
| `orbit_cloud_workers_up` | gauge | | services and consumers running |

```yaml
scrape_configs:
  - job_name: orbit-cloud
    static_configs: [{ targets: ["orbitd:8080"] }]
```

## Console

`GET /` serves the web console (apps, runs, traces, logs, queues); it calls this API with the token you enter. It is
planned to be replaced by an Orin app, see [plan.md](plan.md#control-plane-in-orin). A first version of that app exists in
[`console/`](../console/README.md): it uses only the API above.
