# Orbit Cloud

The platform around the [Orbit](https://github.com/orin-lang/orbit) WebAssembly runtime: deploy a `.wasm` module (an
[Orin](https://github.com/orin-lang/orin-lang) program built with `--target=wasm32-orbit`, or any module that speaks the
Orbit host ABI) and run it as a **job**, a long-running **service** or a **queue consumer**, with **logs**, **traces**,
**metrics**, a **KV database** and **queues** built in. One binary, `orbitd`, with an embedded SQLite database; it runs
as a Docker container or as the only service of a minimal appliance OS image.

Status: milestone 1 (the single-node core) plus the multi-tenant platform layer: organisations, projects, users with
roles, API tokens, and a public gateway that gives every project `<project>.<your-domain>`, takes customers' own
domains and obtains HTTPS certificates by itself. See [docs/plan.md](docs/plan.md) for the plan and the milestones,
[docs/architecture.md](docs/architecture.md) for how it works, [docs/platform.md](docs/platform.md) for projects, auth
and the gateway, and [docs/api.md](docs/api.md) for the HTTP API.

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

## A cloud on your own domain

```sh
# DNS: cloud.example.com and *.cloud.example.com -> this host; ports 80 and 443 open
orbitd --base-domain cloud.example.com --acme-email you@example.com      # data in /var/lib/orbit-cloud
# the log prints:  no users yet. Create the first owner at https://console.cloud.example.com/setup?token=...

occtl login --url https://console.cloud.example.com                       # email + password, saves an API token
occtl projects create shop                                                # -> https://shop.cloud.example.com
occtl -p shop deploy web web.wasm --kind service --http                   # an app that serves HTTP on $PORT
occtl -p shop domains add shop.kunde.de                                   # prints a TXT and a CNAME record to create
```

`ORBIT_CLOUD_TOKEN` stays valid as a break-glass admin token. See [docs/platform.md](docs/platform.md).

## Run it

**Docker** (build context is the repository root):

```sh
docker build -f deploy/docker/Dockerfile -t orbit-cloud .
docker run -d -p 8080:8080 -v orbit-data:/var/lib/orbit-cloud -e ORBIT_CLOUD_TOKEN=secret orbit-cloud
# or: ORBIT_CLOUD_TOKEN=secret docker compose -f deploy/docker/compose.yaml up --build
# with the gateway on 80/443: ORBIT_CLOUD_TOKEN=... ORBIT_CLOUD_BASE_DOMAIN=cloud.example.com \
#   ORBIT_CLOUD_ACME_EMAIL=you@example.com docker compose -f deploy/docker/compose.yaml up -d
```

**systemd**: install `orbitd` to `/usr/bin`, copy `deploy/systemd/orbitd.service` to `/etc/systemd/system/`, put
`ORBIT_CLOUD_TOKEN=...` in `/etc/orbit-cloud/orbitd.env` (mode 0600) and `systemctl enable --now orbitd`. The unit uses
a dynamic user and a sandbox. For the gateway on ports 80 and 443 add the drop-in `deploy/systemd/orbitd-gateway.conf`.

**Appliance image**: `deploy/os/` builds a minimal Debian disk image with mkosi, read-only root and the data on its own
partition; see [deploy/os/README.md](deploy/os/README.md) for building it and booting it in QEMU.

## Layout

| Path | What |
|---|---|
| `crates/oc-store` | SQLite persistence: orgs, projects, users, tokens, apps, deployments, modules, runs, logs, spans, KV, queues, routes, domains, certificates |
| `crates/orbitd` | the node: executor on `orbit-runtime`, supervisor for services and consumers, HTTP API with auth (`auth`, `platform`), the public gateway (`gateway/`), metrics, console |
| `crates/occtl` | the command-line client |
| `docs/` | plan, architecture, platform (projects, auth, gateway), API |
| `deploy/` | Docker (`docker/`), systemd (`systemd/`) and the mkosi appliance OS recipe (`os/`) |

## License

MIT.
