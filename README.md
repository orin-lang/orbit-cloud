# Orbit Cloud

The platform around the [Orbit](https://github.com/orin-lang/orbit) WebAssembly runtime: deploy a `.wasm` module (an
[Orin](https://github.com/orin-lang/orin-lang) program built with `--target=wasm32-orbit`, or any module that speaks the
Orbit host ABI) and run it as a **job**, a long-running **service** or a **queue consumer**, with **logs**, **traces**,
**metrics**, a **KV database** and **queues** built in. One binary, `orbitd`, with an embedded SQLite database; it runs
as a Docker container or as the only service of a minimal appliance OS image.

Status: milestone 1 (the single-node core). See [docs/plan.md](docs/plan.md) for the plan and the milestones,
[docs/architecture.md](docs/architecture.md) for how it works and [docs/api.md](docs/api.md) for the HTTP API.

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
console/run.sh                          # or the console written in Orin: http://127.0.0.1:8081/ (see console/README.md)
```

A queue consumer gets each message on stdin; exit 0 acknowledges it:

```sh
occtl deploy worker worker.wasm --kind consumer --queue orders
occtl queue send orders '{"id": 1}'
occtl logs --app worker -f
```

## Run it

**Docker** (build context is the repository root):

```sh
docker build -f deploy/docker/Dockerfile -t orbit-cloud .
docker run -d -p 8080:8080 -v orbit-data:/var/lib/orbit-cloud -e ORBIT_CLOUD_TOKEN=secret orbit-cloud
# or: ORBIT_CLOUD_TOKEN=secret docker compose -f deploy/docker/compose.yaml up --build
```

**systemd**: install `orbitd` to `/usr/bin`, copy `deploy/systemd/orbitd.service` to `/etc/systemd/system/`, put
`ORBIT_CLOUD_TOKEN=...` in `/etc/orbit-cloud/orbitd.env` (mode 0600) and `systemctl enable --now orbitd`. The unit uses
a dynamic user and a sandbox.

**Appliance image**: `deploy/os/` builds a minimal Debian disk image with mkosi, read-only root and the data on its own
partition; see [deploy/os/README.md](deploy/os/README.md) for building it and booting it in QEMU.

## Layout

| Path | What |
|---|---|
| `crates/oc-store` | SQLite persistence: apps, deployments, modules, runs, logs, spans, KV, queues |
| `crates/orbitd` | the node: executor on `orbit-runtime`, supervisor for services and consumers, HTTP API, metrics, console |
| `crates/occtl` | the command-line client |
| `console/` | the web console as an Orin application on orin-web and orin-ui, a client of the `/v1` API |
| `docs/` | plan, architecture, API |
| `deploy/` | Docker (`docker/`), systemd (`systemd/`) and the mkosi appliance OS recipe (`os/`) |

## License

MIT.
