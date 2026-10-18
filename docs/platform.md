# The multi-tenant platform layer

Milestone 1 is one node with one admin token and a flat list of apps. This page describes the layer that turns it into a
small self-hosted cloud in the style of Vercel or Cloudflare: organisations, projects, real users and API tokens, and a
public gateway that gives every project a hostname, lets customers bring their own domain and terminates HTTPS.

The operator who installs `orbitd` owns a base domain, for example `cloud.example.com`, with wildcard DNS
`*.cloud.example.com` pointing at the node. Every project gets `<project>.cloud.example.com`. A customer adds
`shop.kunde.de` by creating a CNAME to the project host and a TXT record that proves ownership; once verified, it
serves the project over HTTPS with a certificate that `orbitd` obtains by itself.

Status of this document: the design as implemented in the `claude/platform` branch, plus a list of what is deferred
([Not yet](#not-yet)). The HTTP reference for the new endpoints is in [api.md](api.md).

## 1. Model

```
Organization 1---* Project 1---* App (job | service | consumer)
     |                 |
     |                 +--- Routes   (host? + path prefix -> service app)
     |                 +--- Domains  (custom hostnames, verified by DNS)
     +--- Members      (user + role)
```

* **Organization**: the unit of ownership and billing-to-be. Name `[a-z0-9-]{1,63}`, unique on the node.
* **Project**: belongs to one organisation. Its name `[a-z0-9-]{1,63}` is **globally unique** because it is the
  subdomain (`<project>.<base>`). The names `console`, `www`, `api`, `acme`, `admin` and `localhost` are reserved.
* **App**: unchanged (jobs, services, consumers), now inside a project. App names are unique per project.
* **Route**: in a project, `host?` + `path_prefix` -> a service app that declared `"http": true`.
* **Domain**: a custom hostname of a project; `pending` until DNS proves it, then `verified`.

### Names inside the engine

Everything below the API (the Orbit tenant of an isolate, `runs.app`, `logs.app`, `kv.tenant`, `files/<app>/`, the
supervisor's worker map) is keyed by one string, the **app key**:

| App | App key |
|---|---|
| in the `default` project | the bare name, `hello` |
| in any other project | `<project>.<app>`, `shop.web` |

A dot is not a valid character of a name, so keys never collide. Existing data keeps working unchanged: all milestone-1
apps are in the `default` project and their keys did not change. Responses of `/v1/apps` show the short `name` and the
`project`; run and log records keep the key in their `app` field. Queue names get the same treatment
(`<project>.<queue>` outside `default`), so a consumer of project `a` cannot read project `b`'s queue.

### Migration

The store moves to schema version 2 (`crates/oc-store/src/migrate_v2.sql`) on first start: it creates the tables
below, an organisation `default`, a project `default` in it, and assigns every existing app to that project. Nothing
is renamed or copied.

### Schema (version 2 additions)

| Table | Content |
|---|---|
| `orgs` | `id`, `name` (unique), `created_at` |
| `projects` | `id`, `org_id`, `name` (unique), `created_at` |
| `apps` (+columns) | `project_id`, `short` (the name inside the project); unique `(project_id, short)` |
| `users` | `id`, `email` (unique, case-insensitive), `display_name`, `password_hash` (argon2id, nullable for OIDC-only users), `is_admin`, `disabled`, `created_at` |
| `memberships` | `(user_id, org_id)`, `role` |
| `sessions` | `id_hash` (sha256 of the cookie value), `user_id`, `created_at`, `expires_at`, `last_seen`, `ip`, `user_agent` |
| `api_tokens` | `id`, `hash` (sha256), `prefix`, `name`, `user_id` (null: created by the break-glass admin), `scope_kind` (`org`/`project`), `scope_id`, `role`, `expires_at`, `created_at`, `last_used`, `revoked_at` |
| `audit_log` | `id`, `ts`, `actor_kind`, `actor_id`, `actor_label`, `action`, `target`, `org_id`, `project_id`, `detail` (JSON), `ip` |
| `routes` | `id`, `project_id`, `host` (null: every host of the project), `path_prefix`, `app` (key); unique `(project_id, host, path_prefix)` |
| `domains` | `(host, project_id)`, `txt_value`, `status` (`pending`, `verified`), `created_at`, `verified_at`, `last_checked`, `last_error`; a host can be `verified` for one project only |
| `app_ports` | `app` (key), `port` (unique): the loopback port of an HTTP service, stable across restarts |
| `certs` | `host`, `cert_pem`, `key_pem`, `not_after`, `created_at` |
| `acme_accounts` | `directory`, `credentials` (JSON) |

## 2. Authentication and authorisation

### Principals

Every request to `/v1` has exactly one **principal**:

| Principal | How it authenticates | Powers |
|---|---|---|
| **Break-glass admin** | `Authorization: Bearer $ORBIT_CLOUD_TOKEN` | everything, as an owner of everything. Not a user: it has no email, no session, and its actions are audited as `admin-token`. Keep it for recovery and for bootstrapping automation; it keeps working after users exist. |
| **User session** | the `orbit_session` cookie | the user's roles in their organisations |
| **API token** | `Authorization: Bearer oc_...` | the token's role inside its scope |
| **Open node** | no `ORBIT_CLOUD_TOKEN` and no user exists yet | development only, treated as the break-glass admin; `orbitd` logs a warning |

Once the first user exists, a node without `ORBIT_CLOUD_TOKEN` stops being open: requests without credentials get 401.

### Roles

`viewer < developer < admin < owner`, per organisation (membership) or per token scope.

| Permission | Minimum role | Examples |
|---|---|---|
| `Read` | viewer | list/get apps, deployments, runs, traces, logs, KV, queue stats, routes, domains |
| `Write` | developer | deploy, activate, delete an app, run, KV writes, enqueue, create/delete routes |
| `Manage` | admin | domains, project settings, project tokens, org members (not owners), audit log of the org |
| `Own` | owner | delete the org, change owners, delete projects |

Instance-wide actions (create organisations, create users, read the global audit log) need the break-glass admin or a
user with `is_admin` (the first user, created by setup).

### One place that decides: `Authz`

Handlers never look at headers or roles. Each `/v1` handler takes an `Authz` extractor (it authenticates, and answers
401 when it cannot) and asks it one question through one function:

```rust
authz.require(&store, Resource::Project("shop"), Perm::Write)?   // 403 when not allowed
```

`Resource` is `Instance`, `Org(name)` or `Project(name)`. A project resolves to its organisation; a token scoped to
a project only matches that project; a token scoped to an organisation matches all of its projects. Reads of objects
addressed by id (a run, a trace) first map the id to its app and project and then ask the same question. Because the
resource is part of the question, a forbidden project and a missing project both answer 404/403 consistently: the
store lookups happen after the authorisation, with one exception, a project the principal is not a member of is
answered with 404 so that project names cannot be probed.

### Setup

When no user exists, `orbitd` generates a random setup token at start and logs:

```
orbitd: no users yet. Create the first owner at https://console.cloud.example.com/setup?token=<random>
```

`GET /setup` serves a small built-in form; `POST /v1/setup {token, email, display_name, password}` creates the first
user, makes them `is_admin` and owner of the `default` organisation, and opens a session. The token lives in memory
only (a restart prints a new one) and the endpoint answers 409 once a user exists. Setup attempts are rate limited.

### Passwords and sessions

* Passwords: argon2id (RustCrypto `argon2`, default parameters m=19 MiB, t=2, p=1), PHC string in `users`. Hashing
  runs on the blocking pool. A login for an unknown email still performs one hash verification so response time does
  not reveal which emails exist. Minimum length 10.
* Session: 32 random bytes, hex, in the cookie `orbit_session` (`__Host-orbit_session` when the request came in over
  HTTPS): `HttpOnly`, `SameSite=Lax`, `Path=/`, `Secure` when the request was HTTPS (directly or via a trusted
  proxy's `X-Forwarded-Proto`). The database only has `sha256(value)`. Lifetime 14 days, sliding. `POST /v1/auth/logout`
  deletes the row.
* **CSRF**: cookie-authenticated requests with a method other than GET/HEAD/OPTIONS must carry
  `X-CSRF-Token: <token>`, where the token is `sha256("orbit-csrf:" + session value)` in hex, returned by login and
  `GET /v1/me` (the cookie itself is `HttpOnly`, so scripts never see it). If an `Origin` header is present its host must
  equal the `Host` header. Bearer-token requests are not cookie-authenticated and need neither.
* **Rate limit**: failed logins are counted per (client address) and per email in a sliding window (5 failures per
  15 minutes each); beyond that `429` with `Retry-After`. The same limiter guards setup and bad API tokens.

### API tokens

`oc_` + 32 random bytes in base32/hex. Shown once; stored as sha256. A token has a name, a **scope** (`{"org": "acme"}`
or `{"project": "shop"}`), a **role**, an optional expiry, and optionally the user that created it. Rules:

* a user can only create tokens with a role not above their own role in that scope (a token never escalates);
* tokens cannot create tokens; sessions and the break-glass admin can;
* `GET /v1/tokens` lists the caller's tokens (the break-glass admin sees all), `DELETE /v1/tokens/{id}` revokes;
* revoked, expired or belonging-to-a-disabled-user tokens are rejected.

CLI: `occtl login`, `occtl token create|ls|rm`.

### Audit log

Every control-plane mutation appends a row: who (`actor_kind` = `user`, `token`, `admin-token`, `system`), what
(`action` like `project.create`, `app.deploy`, `domain.add`, `token.create`, `user.login`), the target, org, project
and details. `GET /v1/orgs/{org}/audit` (org admin), `GET /v1/audit` (instance admin). Newest first, paged with
`before=<id>`.

### OIDC and passkeys (next steps)

The `users.password_hash` is nullable and `sessions` do not care how the user authenticated, so both fit in
without a schema change. OIDC (authorization code + PKCE, mapping to users by verified email) is **not** in this
change; see [Not yet](#not-yet).

## 3. The gateway

One public listener pair routes by `Host`. Module `orbitd::gateway`.

```
:80  (--http)   ACME http-01 challenges, redirect to https (when TLS is on), or serve (--tls off)
:443 (--https)  rustls, SNI -> certificate, HTTP/1.1 and h2
         |
   host classification (gateway::hosts)
     console.<base> | <base>        -> the API and console (in process, the axum router)
     <project>.<base>               -> project routes
     verified custom domain         -> that domain's project
     anything else                  -> 404 (and never a certificate)
         |
   route table of the project (longest path prefix, host-specific wins a tie)
         |
   reverse proxy -> 127.0.0.1:<app port>
```

### HTTP apps

A service whose spec has `"http": true` gets a loopback port from `orbitd` (range 20000-29999, stored in `app_ports`,
stable). The run gets `PORT=<port>` in its environment and the grant `listen: 127.0.0.1:<port>`; nothing else changes
for the guest. The first HTTP service of a project gets the route `/` automatically if the project has no route
for `/` yet. More routes: `POST /v1/projects/{p}/routes {"path_prefix": "/api", "app": "api", "host": null}`.

### Proxy behaviour

Built on hyper (`hyper-util` legacy client for upstream, auto HTTP/1+2 server). Request and response bodies are
streamed. WebSocket and other `Upgrade` requests are tunnelled. The proxy sets or appends `X-Forwarded-For`,
`X-Forwarded-Proto`, `X-Forwarded-Host`, and `X-Request-Id` (kept when present and sane, generated otherwise; also
returned to the client). Timeouts: header read 30 s, upstream response headers 60 s (`--upstream-timeout-secs`), idle
connections 90 s. Request bodies are limited (`--max-body-mb`, default 100) with 413. A refused connection to the
app is 502, an unknown path 404, a missing app port 503. Hop-by-hop headers are stripped.

**Observability**: every proxied request becomes one log line in the app's logs (`GET /path 200 12ms host=... ip=...`,
level `info`, `warn` for 5xx) and one `http.request` span (attributes: method, path, host, status, duration, bytes,
request id, client ip) in the trace of the app's current service run. They are written by a batch writer, like
guest logs.

### Trusting forwarded headers

`X-Forwarded-*` from a client are overwritten, unless the TCP peer is in `--trusted-proxy` (repeatable CIDR). For a
trusted peer, the client address is the right-most untrusted entry of `X-Forwarded-For`, and `X-Forwarded-Proto` decides
whether cookies are `Secure`. This is the mode for `--tls off`.

### TLS

* `--tls off`: HTTP only, run behind your own reverse proxy / load balancer.
* `--tls auto` (default when `--https` is given). Certificate sources, in order:
  1. a **stored ACME certificate** for exactly that host;
  2. the operator's **wildcard certificate** (`--tls-cert`, `--tls-key`, PEM) when its names cover the host;
  3. **on-demand ACME**: if the host is known and `--acme-email` is set, the handshake waits (up to 60 s) while the
     certificate is ordered (HTTP-01 over `:80`), stored in the `certs` table and used.
* **Only known hosts**: a project host of an existing project, the console host, or a verified custom domain. For any
  other SNI the handshake is refused without contacting the CA; an attacker cannot make the node order certificates or
  fill the database by connecting with random server names. Failed orders back off exponentially per host (1 min up to
  6 h) to stay inside the CA's rate limits.
* The ACME account is created once per directory and stored in `acme_accounts`. Renewal: a background task checks
  daily and renews certificates with less than 30 days left. `--acme-staging` uses Let's Encrypt staging;
  `--acme-directory URL` and `--acme-ca-file` point at another CA (for example Pebble in tests).
* Challenge type: HTTP-01 (needs port 80 reachable on the node). TLS-ALPN-01 is a possible addition, it needs no
  code outside `gateway::tls`.

### Custom domains

1. `POST /v1/projects/{p}/domains {"host": "shop.kunde.de"}` (admin). The response contains the instructions:
   `cname`: `{name: "shop.kunde.de", value: "<project>.<base>"}` and `txt`: `{name: "_orbit-verify.shop.kunde.de",
   value: "orbit-verify=<random>"}`.
2. The customer creates both records. (An apex domain cannot have a CNAME; use the provider's ALIAS/ANAME or A/AAAA
   records to the node. Only the TXT record is required for verification, the CNAME/address check is informational.)
3. A background verifier (every 30 s for pending domains, with the first check right after creation, and
   `POST .../domains/{host}/verify` to check now) resolves the TXT record with `hickory-resolver`; on a match
   the domain becomes `verified` and the certificate is ordered in the background.
4. `GET /v1/projects/{p}/domains` shows `status`, `last_checked`, `last_error`, and `cname_ok` as last seen.

Pending claims are per (host, project) and verification needs the project's own random TXT value, so squatting a
name before its owner adds it gets nobody anything. Hosts under the base domain, IP addresses and syntactically invalid
names are refused.

## 4. Configuration

| Flag / environment | Meaning |
|---|---|
| `--base-domain`, `ORBIT_CLOUD_BASE_DOMAIN` | enables the gateway's host routing |
| `--http`, `ORBIT_CLOUD_HTTP` | public HTTP listener, e.g. `0.0.0.0:80` |
| `--https`, `ORBIT_CLOUD_HTTPS` | public HTTPS listener, e.g. `0.0.0.0:443` |
| `--tls auto\|off` | see above |
| `--tls-cert`, `--tls-key` | operator wildcard certificate (PEM) |
| `--acme-email`, `--acme-staging`, `--acme-directory`, `--acme-ca-file` | ACME |
| `--trusted-proxy CIDR` | trust `X-Forwarded-*` from these peers |
| `--max-body-mb`, `--upstream-timeout-secs` | proxy limits |
| `--listen` (existing) | direct API/console listener; with a gateway bind it to loopback |
| `ORBIT_CLOUD_TOKEN` (existing) | the break-glass admin token |

Without `--base-domain` there is no gateway and `orbitd` behaves as before on `--listen`.

## 5. API calls the console needs

The Orin console (on `claude/orin-console`) can build on these; bodies are in [api.md](api.md#platform).

| Screen | Calls |
|---|---|
| First start | `GET /setup` is served by orbitd. `POST /v1/setup` |
| Login | `POST /v1/auth/login {email, password}` -> cookie + `{user, csrf_token}`; `GET /v1/me` on page load (401 = show login); `POST /v1/auth/logout` |
| Every mutation | send `X-CSRF-Token` from login/`/v1/me` with the cookie |
| Project switcher | `GET /v1/me` (memberships), `GET /v1/projects`, `POST /v1/projects {org, name}` |
| Apps of a project | `GET /v1/projects/{p}/apps`, `GET /v1/projects/{p}/apps/{a}`, deployments, runs, logs, KV: the same paths as `/v1/apps/...` below `/v1/projects/{p}` |
| Live logs | `GET /v1/projects/{p}/logs/stream` (cookie auth works for EventSource) |
| Domains page | `GET/POST /v1/projects/{p}/domains`, `POST .../domains/{host}/verify`, `DELETE .../domains/{host}` |
| Routes | `GET/POST /v1/projects/{p}/routes`, `DELETE /v1/projects/{p}/routes/{id}` |
| Members | `GET/POST /v1/orgs/{o}/members`, `PATCH/DELETE .../members/{user}` |
| API tokens | `GET/POST /v1/tokens`, `DELETE /v1/tokens/{id}` |
| Audit | `GET /v1/orgs/{o}/audit` |

## Not yet

* **OIDC login** (generic provider, authorization code + PKCE, user mapping by verified email). The session and user
  tables are ready; what is missing is the `/v1/auth/oidc/*` endpoints and provider configuration.
* **GitHub OAuth**, **passkeys / WebAuthn** (store credentials in a `webauthn_credentials` table, add as a second factor or a
  primary method, issue the same session), **email invitations**, password reset by email, TOTP.
* **TLS-ALPN-01** and DNS-01 (wildcards by ACME).
* ACME has no integration test against a real CA in CI (Pebble is not available there); the plumbing (known-host
  gate, challenge responses, back-off, certificate selection, storage) is unit tested, see `gateway::tls`.
* Quotas and billing per organisation, per-project secrets, per-project queues API (queue names are prefixed already).
* Multi-node gateways (certificates live in the node's store).
