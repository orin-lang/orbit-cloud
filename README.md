# Orbit Cloud

The platform around the [Orbit](https://github.com/orin-lang/orbit) WebAssembly runtime: deploy a `.wasm` module (an
[Orin](https://github.com/orin-lang/orin-lang) program built with `--target=wasm32-orbit`, or any module that speaks the
Orbit host ABI) and run it as a **job**, a long-running **service** or a **queue consumer**, with **logs**, **traces**,
**metrics**, a **KV database** and **queues** built in. One binary, `orbitd`, with an embedded SQLite database; it runs
as a Docker container or as the only service of a minimal appliance OS image.

Status: milestone 1 in progress. See [docs/plan.md](docs/plan.md) for the plan and the milestones.

## Quick start

```sh
cargo build --release
ORBIT_CLOUD_TOKEN=secret target/release/orbitd --listen 127.0.0.1:8080 --data-dir ./data &

export ORBIT_CLOUD_URL=http://127.0.0.1:8080 ORBIT_CLOUD_TOKEN=secret
orin build --target=wasm32-orbit hello.or -o hello.wasm
target/release/occtl deploy hello hello.wasm
target/release/occtl run hello          # prints the run and its log lines
target/release/occtl trace <run-id>     # load / instantiate / execute spans
open http://127.0.0.1:8080/             # web console
```

A queue consumer gets each message on stdin; exit 0 acknowledges it:

```sh
occtl deploy worker worker.wasm --kind consumer --queue orders
occtl queue send orders '{"id": 1}'
occtl logs --app worker -f
```

## Layout

| Path | What |
|---|---|
| `crates/oc-store` | SQLite persistence: apps, deployments, modules, runs, logs, spans, KV, queues |
| `crates/orbitd` | the node: executor on `orbit-runtime`, supervisor for services and consumers, HTTP API, metrics, console |
| `crates/occtl` | the command-line client |
| `docs/` | plan, architecture, API |
| `deploy/` | Docker, systemd and the appliance OS recipe |

## License

MIT.
