# PostgreSQL for apps

Apps get **PostgreSQL** as their database. orbitd does not run PostgreSQL; the operator provides a server and orbitd
provisions a database and a login role per app on it.

## Enabling it

Give orbitd an admin connection:

```sh
orbitd --postgres-url postgres://postgres:SECRET@db.internal:5432/postgres      # or ORBIT_CLOUD_POSTGRES_URL=...
```

The role in the URL must be a superuser or, on PostgreSQL 16 and later, have `CREATEROLE` and `CREATEDB`. orbitd uses it
only to create, inspect and drop what it provisions; apps never see it. The connection is plain TCP (no TLS), so keep the
server on a private network (compose: the `postgres` service is not published; the appliance image: loopback).
Docker compose (`deploy/docker/compose.yaml`) already runs `postgres:17` and wires the URL; the appliance recipe is in
`deploy/os/README.md`.

Then name it in the spec:

```json
{ "kind": "service", "postgres": true, "listen": ["0.0.0.0:9000"] }
```

```sh
occtl deploy shop shop.wasm --postgres
```

## What a deploy does

For each deploy with `"postgres": true`, before the deployment is recorded and idempotently:

1. creates the role `oc_<app>` (`-` becomes `_`; longer than 63 bytes: cut plus a hash) with a random 32-character
   password: `LOGIN`, no superuser/createdb/createrole/replication/bypassrls, `CONNECTION LIMIT 25`, and a comment that
   marks it as orbit-cloud's;
2. creates the database `oc_<app>` owned by that role and revokes `CONNECT` from `PUBLIC`, so no other app's role can enter;
3. stores the password in orbitd's store.

A role of that name that orbitd did not create is never taken over (the deploy fails). An interrupted first deploy is
repaired by the next one (the role gets a fresh password). Every identifier and literal in the generated SQL is quoted
(`provision::quote_ident` / `quote_literal`); the statements are built by pure functions with unit tests, including hostile names.

At every run of the app orbitd adds to its environment

```
DATABASE_URL=postgres://oc_shop:PASSWORD@db.internal:5432/oc_shop
```

(it wins over a `DATABASE_URL` in the spec's `env`) and adds the server's `HOST:PORT` to the `connect` grant of
`orbit:net/v1`. Neither is stored in the spec, so `GET /v1/apps/{app}` never shows the password. Apps built with
[orin-psql](https://github.com/orin-lang/orin-psql) take the URL with `parseUrl`. Orin's std cannot read environment
variables yet, so an Orin app currently receives the URL as a program argument it builds itself or reads from a file; WASI
guests (Rust, C) read `DATABASE_URL` directly.

## Status, deleting

`GET /v1/apps/{app}/database` (CLI `occtl db APP [--password]`): database, role, host, port, URL without password, size,
open connections. Deleting an app **keeps** its database and role (data is not lost by accident, and a redeploy finds it);
`DELETE /v1/apps/{app}?purge=true` (CLI `occtl delete APP --purge`) drops the database (terminating its sessions) and the role
and forgets the password. Purge works for a name whose app is already gone.

## Follow-ups

* The password is stored in the SQLite store in clear text. Encrypting it at rest (a node key from a credential/KMS) is not done.
* Projects: names are produced in one place (`provision::names`); a project scope will add a distinct prefix there.
* Rotating an app's password, connection pooling, per-app limits on size, TLS to the server, and a console panel are not built.
* Backups are the operator's job (`pg_dump`/WAL archiving of the server).

## Testing

`cargo test` runs unit tests of the planning/quoting code. The tests that need a server run when
`ORBIT_CLOUD_TEST_POSTGRES_URL` holds a superuser URL (CI starts a `postgres:17` service for it):
`ORBIT_CLOUD_TEST_POSTGRES_URL=postgres://postgres:secret@127.0.0.1:5432/postgres cargo test -p orbitd --test postgres`.
