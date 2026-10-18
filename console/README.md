# Orbit Cloud console, in Orin

The web console of Orbit Cloud written as an [Orin](https://github.com/orin-lang/orin-lang) application, on the libraries
[orin-web](https://github.com/orin-lang/orin-web) (HTTP framework) and [orin-ui](https://github.com/orin-lang/orin-ui) (UI with a
live patch stream). It replaces nothing in `orbitd` yet: the Rust daemon still serves its own static console at `/`. This one runs
as a separate program and talks to orbitd's `/v1` API like any Orbit Cloud app could (it needs one `connect` grant, to orbitd).

Screens, in a Cloudflare-style layout (left sidebar, top bar with the node status, theme toggle and user menu), light and dark:

* **Overview**: stat cards (apps, runs in 24 h with the change against the day before, error rate, p95 run time, queue backlog, dead
  letters, active workers, log lines), a stacked runs-per-hour bar chart (succeeded versus failed), a run-duration chart (p50 and
  p95), the latest failed runs and the queues that need attention.
* **Apps**: a sortable table with kind, version, last run, a 24 hour sparkline and the worker state.
* **An app**, in tabs: Overview (metrics, chart, recent runs), Deployments (versions, the spec as Limits / Capabilities / Environment
  groups or raw JSON, a diff against the previous version, activate behind a confirm dialog), Runs, Logs (live, filtered to the app),
  KV (namespaces, entries, a value viewer) and Settings (delete, with the app's name typed to confirm).
* **A run**: status, execution time in microseconds or milliseconds, memory, a span waterfall on a labelled time axis, the host calls
  and the log lines with level colours.
* **Logs**: filter bar (app, level, text), live follow with pause, monospaced coloured lines.
* **Queues**: a card per queue with ready / leased / dead / done, a dead-letter table with redrive, publish in a dialog.
* **KV** (all namespaces) and **Settings** (the node, this console, the theme).

The components (shell, sidebar, tables, tabs, dialogs, charts) are not part of this package: they are the
[orin-ui component kit](https://github.com/orin-lang/orin-ui#component-kit), and the charts are SVG generated in Orin. The data for
the charts comes from orbitd's `GET /v1/stats`.

## Run it

```sh
cargo build --release
ORBIT_CLOUD_TOKEN=secret target/release/orbitd --listen 127.0.0.1:8080 --data-dir ./data &

ORBIT_CLOUD_TOKEN=secret console/run.sh          # console on http://127.0.0.1:8081
# or by hand, from console/:
orin fetch
orin run -- --api http://127.0.0.1:8080 --api-token-file ./token --listen 127.0.0.1:8081
```

Open <http://127.0.0.1:8081/> and sign in with the console token (by default the orbitd token). Options (`orin run -- ...`):

| Option | Meaning |
|---|---|
| `--listen ADDR:PORT` | where the console listens (default `127.0.0.1:8081`) |
| `--api URL` | orbitd, `http://HOST:PORT` (default `http://127.0.0.1:8080`); no TLS |
| `--api-token TOKEN`, `--api-token-file FILE` | bearer token for orbitd |
| `--console-token TOKEN`, `--console-token-file FILE` | what signs in to the console; default: the orbitd token |
| `--assets DIR`, `--client-dir DIR` | `console.css` (default `assets`) and orin-ui's `client/` directory with `orin-ui.js`, `orin-ui-kit.css` and `orin-ui-kit.js` (default: the copy in the fetched dependency) |
| `--workers N` | parallel requests to orbitd (default 4) |
| `--quiet` | no access log |

Orin's std has no access to environment variables, so tokens come from arguments (visible in `ps`) or token files; `run.sh` turns
`ORBIT_CLOUD_TOKEN` / `CONSOLE_TOKEN` into files for you.

**Signing in.** `/login` takes the console token and sets an opaque `orbit_console` cookie (`HttpOnly`, `SameSite=Strict`);
`/logout` revokes it. For scripts: `Authorization: Bearer <console token>`, or `GET /any/page?token=<console token>`, which sets
the cookie and redirects (the token then sits in the URL once; prefer the bearer header). There is no TLS: put the console behind
a TLS proxy before exposing it, and mark the cookie `Secure` there. Login tokens come from a pool of 4096 random bytes read from
`/dev/urandom` at start-up (Orin has no random generator); after 256 sign-ins restart the console.

## How it fits together

```
 browser ──HTTP──► orbit-console (Orin) ──HTTP /v1──► orbitd (Rust) ──► Orbit runtime, SQLite
   orin-ui.js        orin-web router + auth               bearer token
   (patches)         orin-ui program per page
```

* **Pages are server-rendered** by orin-ui (`renderHtml`) from a `Model`; links are plain links. Each page opens a **session** in the
  hub task and the browser attaches `orin-ui.js` to it.
* **Events** (`click`, form `submit`) are POSTed to `/ui/events`; the console resolves the element to a typed `Msg`, performs the API
  calls for it (`effects.or`), applies the resulting messages with the pure `update`, and answers with the **patch** between the old
  and the new screen. Nothing reloads.
* **Live logs.** Handlers have no timers (a closure cannot hold a clock grant), so `/ui/stream` is one poll of orbitd's log per
  request, delivered as server-sent events with a `retry:` hint; the browser's `EventSource` reconnects every 0.5 s while following.
  Patches from the stream and from event replies are numbered, and the client applies them in order (orin-ui's "ordered delivery").
* **State.** Orin tasks share no mutable state: one **hub** task owns the sessions and logins, API **worker** tasks own the network
  grant (a handler cannot capture a grant), and handlers reach both through channels. The views, effects and routes only see an
  `Api` function, so tests swap orbitd for canned answers.

| File | |
|---|---|
| `src/main.or`, `config.or` | arguments, files, grants, tasks, `serve` |
| `src/transport.or` | HTTP/1.1 client on `std:net` + `std:http`, worker pool, `netApi` |
| `src/api.or`, `decode.or`, `jv.or`, `types.or` | typed calls to `/v1`, tolerant JSON decoding |
| `src/model.or`, `effects.or`, `program.or` | the orin-ui program: model, messages, API effects (loading screens, acting on messages) |
| `src/views*.or`, `fmt.or` | the screens, built from the kit (`views_common.or`: shell, run tables, log lines; one file per area) and number/time formatting |
| `src/hub.or`, `app.or`, `page.or` | sessions and logins, routes and auth, the HTML shell |

## Dependencies

`orin.toml` pins both libraries to commits as git dependencies (`orin fetch` downloads them into `.orin/packages`, `orin.lock`
records them). orin-web's repository is private: anything that runs `orin fetch` (CI, a fresh machine) needs git credentials for
`github.com/orin-lang/orin-web`, e.g. `git config --global url."https://x-access-token:$TOKEN@github.com/".insteadOf
"https://github.com/"` with a read-only token. For local work against checkouts replace a line by `{ path = "../../orin-web" }`.

## Demo

```sh
console/tools/demo.sh                    # orbitd + console with demo data on 127.0.0.1:18080 / 18081, token "demo"; Ctrl-C cleans up
console/tools/demo.sh --shots DIR        # light and dark screenshots of every screen (needs Playwright), then stops
```

Seven apps (jobs, a service, two consumers), two versions of `etl` with KV data, a failing job, a consumer whose messages become
dead letters, and 36 hours of runs, spans and log lines that `tools/backfill.py` (a dev-only helper) writes into orbitd's SQLite file
so that the charts have a history. The demo programs in `tools/demo/*.or` are plain `console.log` programs.

## Tests

Needs the Orin toolchain **built from `orin-lang` main**: the latest release (0.1.0-beta.2) predates the std API used here
(`Sender.send` returning `Result`), so there is no CI job for this package yet.

```sh
cd console
orin fetch && orin test               # 106 tests: routes through orin-web's TestClient against a fake orbitd, orin-ui view tests
                                      #  (shell, tables, forms, dialogs, charts, patches), the HTTP client over loopback sockets,
                                      #  model, effects, hub, decoders, formatting
orin fmt --check
tools/e2e.sh                          # real orbitd + console on 127.0.0.1:18080/18081, checks with curl; with
                                      #  PLAYWRIGHT=/path/to/node_modules/playwright (and CHROMIUM=...) it also drives a browser
```

## WebAssembly (`wasm32-orbit`)

`orin build --target=wasm32-orbit` of the whole program fails: `std:net` (listening, connecting) and `std:fs` (`main.or` reads the
assets, the token files and `/dev/urandom`) are not available on the target (E0523). The rest is portable: `tools/wasm_check.or`
runs the decoders, effects, model and views for wasm32-orbit and prints byte-for-byte what the native build prints. What a wasm
deployment needs: `std:net`/`std:http` client (orin-lang `claude/orbit-l0-net`) for the `Api`, a way to receive requests (orbitd
itself would host the console), assets compiled in or provided by the host, and tasks that cooperate: the hub and the API workers
are tasks that wait for each other, which the one-task-at-a-time wasm model deadlocks on, so on wasm they would have to become plain
function calls (`Hub` and `Api` are already the only seams).
